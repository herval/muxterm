//! Read-only Conductor project discovery. Scripts are data, never executed.
use crate::workspace::Project;
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
};

#[derive(Debug)]
pub struct Candidate {
    pub project: Project,
    pub warnings: Vec<String>,
    pub selected: bool,
}

#[derive(Default)]
pub struct ImportState {
    pub pending: Option<mpsc::Receiver<Result<Vec<Candidate>, String>>>,
    pub candidates: Vec<Candidate>,
    pub message: Option<String>,
}

impl ImportState {
    pub fn start(&mut self, ctx: egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.candidates.clear();
        self.message = Some("Reading Conductor projects...".into());
        std::thread::spawn(move || {
            let result = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("home directory unavailable"))
                .and_then(|home| discover(&home))
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    pub fn poll(&mut self) {
        let Some(rx) = &self.pending else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Conductor reader stopped unexpectedly".into())
            },
        };
        self.pending = None;
        match result {
            Ok(rows) => {
                self.message = Some(format!(
                    "{} projects found. Review before importing.",
                    rows.len()
                ));
                self.candidates = rows;
            },
            Err(error) => self.message = Some(error),
        }
    }
}

#[derive(Deserialize)]
struct Repository {
    name: String,
    root_path: String,
    default_branch: Option<String>,
    remote: Option<String>,
    setup_script: Option<String>,
    archive_script: Option<String>,
    run_script: Option<String>,
    file_include_globs: Option<String>,
    conductor_config: Option<String>,
}

pub fn duplicate(project: &Project, existing: &[Project]) -> bool {
    existing.iter().any(|p| {
        p.name == project.name
            || canonical(&p.local_root()) == canonical(&project.local_root())
    })
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn read_toml(path: &Path) -> anyhow::Result<Option<toml::Value>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.parse().map_err(|_| {
            anyhow::anyhow!("invalid TOML in {}", path.display())
        })?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
    }
}

fn merge(target: &mut serde_json::Value, source: serde_json::Value) {
    match (target, source) {
        (
            serde_json::Value::Object(target),
            serde_json::Value::Object(source),
        ) => {
            for (key, value) in source {
                merge(
                    target.entry(key).or_insert(serde_json::Value::Null),
                    value,
                );
            }
        },
        (target, source) => *target = source,
    }
}

fn string_at<'a>(
    value: &'a serde_json::Value,
    section: &str,
    key: &str,
) -> Option<&'a str> {
    value.get(section)?.get(key)?.as_str()
}

fn candidate(
    repo: Repository,
    global: &serde_json::Value,
) -> anyhow::Result<Candidate> {
    let root = PathBuf::from(&repo.root_path);
    anyhow::ensure!(
        root.is_absolute(),
        "{} has a non-absolute repository path",
        repo.name
    );
    let mut config = global.clone();
    if let Some(legacy) =
        repo.conductor_config.as_deref().filter(|s| !s.is_empty())
    {
        merge(
            &mut config,
            serde_json::from_str(legacy).map_err(|_| {
                anyhow::anyhow!("invalid legacy settings for {}", repo.name)
            })?,
        );
    }
    for filename in ["settings.toml", "settings.local.toml"] {
        if let Some(value) = read_toml(&root.join(".conductor").join(filename))?
        {
            merge(&mut config, serde_json::to_value(value)?);
        }
    }
    let mut warnings = Vec::new();
    for (label, present) in [
        (
            "Archive script",
            string_at(&config, "scripts", "archive")
                .or(repo.archive_script.as_deref())
                .is_some_and(|s| !s.is_empty()),
        ),
        (
            "Run scripts",
            config.get("scripts").and_then(|v| v.get("run")).is_some()
                || repo.run_script.as_deref().is_some_and(|s| !s.is_empty()),
        ),
        ("Custom prompts", config.get("prompts").is_some()),
    ] {
        if present {
            warnings.push(format!("{label} will not be imported"));
        }
    }
    warnings
        .push("Agent/model and archive preferences remain unchanged".into());
    warnings.push(
        "Environment variables and env-file settings are not imported".into(),
    );
    let copy_files = if let Some(patterns) = &repo.file_include_globs {
        // Older schemas store newline text; some releases store a JSON array.
        serde_json::from_str::<Vec<String>>(patterns).unwrap_or_else(|_| {
            patterns
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
    } else {
        // The current Conductor UI defaults to .env* when no override is stored.
        warnings.push(
            "Copy patterns default to .env*; verify for this project".into(),
        );
        vec![".env*".into()]
    };
    let remote = repo
        .remote
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("origin");
    let branch = string_at(&config, "git", "default_branch")
        .or(repo.default_branch.as_deref())
        .filter(|s| !s.is_empty());
    let default_branch = branch.map(|branch| {
        if branch.starts_with("refs/")
            || branch.starts_with(&format!("{remote}/"))
        {
            branch.to_string()
        } else {
            format!("{remote}/{branch}")
        }
    });
    let setup = string_at(&config, "scripts", "setup")
        .or(repo.setup_script.as_deref())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    if setup.as_deref().is_some_and(|s| s.contains("CONDUCTOR_")) {
        warnings.push("Setup uses CONDUCTOR_ variables; adapt before creating a workspace".into());
    }
    let selected = root.is_dir();
    if !selected {
        warnings.push("Repository folder is missing; not selected".into());
    }
    Ok(Candidate {
        project: Project {
            name: repo.name,
            path: Some(root),
            repo: None,
            setup,
            subdir: None,
            default_branch,
            copy_files,
        },
        warnings,
        selected,
    })
}

fn discover(home: &Path) -> anyhow::Result<Vec<Candidate>> {
    let database =
        home.join("Library/Application Support/com.conductor.app/conductor.db");
    anyhow::ensure!(database.is_file(), "Conductor database not found");
    let out = Command::new("sqlite3").args(["-readonly", "-json"]).arg(database).arg(
        "SELECT name, root_path, default_branch, remote, setup_script, archive_script, run_script, file_include_globs, conductor_config FROM repos WHERE hidden=0 ORDER BY display_order"
    ).output()?;
    anyhow::ensure!(out.status.success(), "Could not read Conductor repository list (unsupported database schema or locked database)");
    let repos: Vec<Repository> = if out.stdout.is_empty() {
        vec![]
    } else {
        serde_json::from_slice(&out.stdout)?
    };
    let global = read_toml(&home.join(".conductor/settings.toml"))?
        .map(serde_json::to_value)
        .transpose()?
        .unwrap_or_else(|| serde_json::json!({}));
    repos
        .into_iter()
        .map(|repo| candidate(repo, &global))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "reads locally installed Conductor configuration; opt-in smoke test"]
    fn installed_conductor_preview() {
        let rows = discover(&dirs::home_dir().unwrap()).unwrap();
        assert!(!rows.is_empty());
        for row in rows {
            println!(
                "{}: base={}, setup={}, copy={}, warnings={}",
                row.project.name,
                row.project.default_branch.as_deref().unwrap_or("HEAD"),
                row.project.setup.is_some(),
                row.project.copy_files.join(","),
                row.warnings.len()
            );
        }
    }

    #[test]
    fn local_settings_override_legacy_and_duplicates_preserve_existing() {
        let root = std::env::temp_dir()
            .join(format!("mux-conductor-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join(".conductor")).unwrap();
        fs::write(
            root.join(".conductor/settings.toml"),
            "[scripts]\nsetup = 'shared'\n",
        )
        .unwrap();
        fs::write(
            root.join(".conductor/settings.local.toml"),
            "[scripts]\nsetup = 'local'\narchive = 'cleanup'\n",
        )
        .unwrap();
        let repo: Repository = serde_json::from_value(serde_json::json!({"name":"demo","root_path":root,"default_branch":"prod","conductor_config":"{\"scripts\":{\"setup\":\"legacy\"}}"})).unwrap();
        let row =
            candidate(repo, &serde_json::json!({"scripts":{"setup":"global"}}))
                .unwrap();
        assert_eq!(row.project.setup.as_deref(), Some("local"));
        assert_eq!(row.project.default_branch.as_deref(), Some("origin/prod"));
        assert_eq!(row.project.copy_files, vec![".env*"]);
        assert!(row.warnings.iter().any(|w| w.contains("Archive")));
        let mut renamed = row.project.clone();
        renamed.name = "another name".into();
        assert!(duplicate(&row.project, &[renamed]));
        fs::remove_dir_all(root).unwrap();
    }
}
