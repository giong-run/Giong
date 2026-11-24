//! `giong db` against real databases in Docker or Podman: up, new, migrate,
//! the refusals (a changed file, a failing file rolled back), reset, down.
//! Skipped, with a message, where no container runtime answers;
//! `GIONG_DB_REQUIRED=1` turns the skip into a failure.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn runtime() -> Option<String> {
    let candidates = match std::env::var("GIONG_CONTAINER_RUNTIME") {
        Ok(r) if !r.is_empty() => vec![r],
        _ => vec!["docker".to_string(), "podman".to_string()],
    };
    candidates.into_iter().find(|c| {
        Command::new(c)
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn skip() -> bool {
    if runtime().is_some() {
        return false;
    }
    assert!(
        std::env::var("GIONG_DB_REQUIRED").is_err(),
        "GIONG_DB_REQUIRED is set and neither Docker nor Podman answers"
    );
    eprintln!("skipped: neither Docker nor Podman answers");
    true
}

/// A project directory with a unique package name, cleaned up (database
/// included) when dropped.
struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(tag: &str, deps: &str) -> Project {
        let name = format!("giongclitest-{tag}-{}", std::process::id());
        let dir = std::env::temp_dir().join(&name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[dependencies]\n{deps}"),
        )
        .unwrap();
        std::fs::write(dir.join(".env.example"), "PORT=8080\nDATABASE_URL=\n").unwrap();
        Project { dir }
    }

    fn giong(&self, args: &[&str]) -> (i32, Value) {
        let out = Command::new(env!("CARGO_BIN_EXE_giong"))
            .args(args)
            .arg("--json")
            .current_dir(&self.dir)
            .env_remove("DATABASE_URL")
            .env_remove("REDIS_URL")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let v = serde_json::from_str(&text).unwrap_or_else(|e| {
            panic!(
                "not JSON ({e}): {text}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code().unwrap_or(-1), v)
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn append(&self, rel: &str, text: &str) {
        let old = std::fs::read_to_string(self.dir.join(rel)).unwrap();
        self.write(rel, &(old + text));
    }

    fn env(&self, key: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join(".env")).ok()?;
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_string))
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_giong"))
            .args(["db", "down", "--volumes"])
            .current_dir(&self.dir)
            .output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn finding_codes(r: &Value) -> Vec<String> {
    r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap().to_string())
        .collect()
}

fn psql_in(p: &Project, sql: &str) -> String {
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(p.dir.join(".giong/db.json")).unwrap())
            .unwrap();
    let s = &state["postgres"];
    let out = Command::new(s["runtime"].as_str().unwrap())
        .args(["exec", "-e"])
        .arg(format!("PGPASSWORD={}", s["password"].as_str().unwrap()))
        .arg(s["container"].as_str().unwrap())
        .args(["psql", "-At", "-h", "127.0.0.1", "-U", "giong", "-d"])
        .arg(s["database"].as_str().unwrap())
        .args(["-c", sql])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn exists(p: &Path) -> bool {
    p.exists()
}

#[test]
fn postgres_from_up_to_down() {
    if skip() {
        return;
    }
    let p = Project::new("pg", "giong-postgres = \"1.5\"\n");
    let (s, r) = p.giong(&["db", "up"]);
    assert_eq!(s, 0, "{r}");
    let url = p.env("DATABASE_URL").expect("DATABASE_URL in .env");
    assert!(url.starts_with("postgres://giong:"), "{url}");
    // .env kept the other key and is ignored by git, as is .giong/.
    assert_eq!(p.env("PORT").as_deref(), Some("8080"));
    let ignore = std::fs::read_to_string(p.dir.join(".gitignore")).unwrap();
    assert!(
        ignore.contains(".env") && ignore.contains(".giong/"),
        "{ignore}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(p.dir.join(".giong/db.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "db.json holds a password: {mode:o}");
    }
    // Up again reuses it.
    let (s, r) = p.giong(&["db", "up"]);
    assert_eq!(s, 0, "{r}");
    assert_eq!(r["data"]["databases"][0]["action"], "already running");
    assert_eq!(p.env("DATABASE_URL").unwrap(), url);

    let (s, r) = p.giong(&["db", "new", "Create Users"]);
    assert_eq!(
        (s, r["data"]["file"].as_str()),
        (0, Some("migrations/0001_create_users.sql"))
    );
    p.append(
        "migrations/0001_create_users.sql",
        "CREATE TABLE users (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL);\n\
         INSERT INTO users (name) VALUES ('semi;colon');\n",
    );
    let (_, r) = p.giong(&["db", "new", "add_email"]);
    assert_eq!(r["data"]["version"], 2);
    p.append(
        "migrations/0002_add_email.sql",
        "ALTER TABLE users ADD COLUMN email TEXT;\n",
    );

    let (s, r) = p.giong(&["db", "status"]);
    assert_eq!(s, 0, "{r}");
    assert_eq!(r["data"]["pending"].as_array().unwrap().len(), 2);
    let (s, r) = p.giong(&["db", "migrate"]);
    assert_eq!(s, 0, "{r}");
    assert_eq!(r["data"]["applied"].as_array().unwrap().len(), 2);
    assert_eq!(psql_in(&p, "SELECT name FROM users"), "semi;colon");
    let (s, r) = p.giong(&["db", "migrate"]);
    assert_eq!((s, r["data"]["applied"].as_array().unwrap().len()), (0, 0));

    // A changed applied file: nothing runs, coded error, exit 1.
    p.append("migrations/0002_add_email.sql", "-- edited\n");
    p.write("migrations/0003_more.sql", "CREATE TABLE more (x int);\n");
    let (s, r) = p.giong(&["db", "migrate"]);
    assert_eq!(s, 1, "{r}");
    assert!(
        finding_codes(&r).contains(&"GIONG-E5043".to_string()),
        "{r}"
    );
    assert_eq!(psql_in(&p, "SELECT to_regclass('more') IS NULL"), "t");
    let text = std::fs::read_to_string(p.dir.join("migrations/0002_add_email.sql")).unwrap();
    p.write(
        "migrations/0002_add_email.sql",
        text.trim_end_matches("-- edited\n"),
    );

    // A failing file is rolled back whole and points at its line.
    p.write(
        "migrations/0003_more.sql",
        "CREATE TABLE more (x int);\nSELECT * FROM missing_table;\n",
    );
    let (s, r) = p.giong(&["db", "migrate"]);
    assert_eq!(s, 1, "{r}");
    let f = &r["findings"][0];
    assert_eq!(f["code"], "GIONG-E5044", "{r}");
    assert_eq!(f["file"], "migrations/0003_more.sql");
    assert_eq!(f["line"], 2, "{r}");
    assert_eq!(psql_in(&p, "SELECT to_regclass('more') IS NULL"), "t");

    // A no-transaction file.
    p.write(
        "migrations/0003_more.sql",
        "-- giong:no-transaction\nCREATE INDEX CONCURRENTLY users_name ON users (name);\n",
    );
    let (s, r) = p.giong(&["db", "migrate"]);
    assert_eq!(s, 0, "{r}");

    // Reset needs --yes, then starts over from the files.
    let (s, r) = p.giong(&["db", "reset"]);
    assert_eq!((s, r["error"]["code"].as_str()), (2, Some("GIONG-E5000")));
    psql_in(&p, "INSERT INTO users (name) VALUES ('extra')");
    let (s, r) = p.giong(&["db", "reset", "--yes"]);
    assert_eq!(s, 0, "{r}");
    assert_eq!(psql_in(&p, "SELECT count(*) FROM users"), "1");
    assert_eq!(psql_in(&p, "SELECT count(*) FROM giong_migrations"), "3");

    // Down keeps the data, and the credentials with it: up brings both back.
    let (s, r) = p.giong(&["db", "down"]);
    assert_eq!(s, 0, "{r}");
    let (_, r) = p.giong(&["db", "status"]);
    assert_eq!(r["data"]["databases"][0]["state"], "removed", "{r}");
    let (s, r) = p.giong(&["db", "up"]);
    assert_eq!(s, 0, "{r}");
    assert_eq!(r["data"]["databases"][0]["action"], "created");
    assert_eq!(p.env("DATABASE_URL").unwrap(), url);
    assert_eq!(psql_in(&p, "SELECT count(*) FROM users"), "1");
    // Data whose password was lost is refused, not silently broken.
    std::fs::remove_file(p.dir.join(".giong/db.json")).unwrap();
    let (s, r) = p.giong(&["db", "down"]);
    assert_eq!(s, 0, "{r}");
    let (s, r) = p.giong(&["db", "up"]);
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (2, Some("GIONG-E5049")),
        "{r}"
    );
    let (s, r) = p.giong(&["db", "down", "--volumes"]);
    assert_eq!(s, 0, "{r}");
    assert!(
        !r["data"]["volumes_removed"].as_array().unwrap().is_empty(),
        "{r}"
    );
    assert!(!exists(&p.dir.join(".giong/db.json")));
    let (s, r) = p.giong(&["db", "up"]);
    assert_eq!(s, 0, "{r}");
}

#[test]
fn redis_up_writes_redis_url() {
    if skip() {
        return;
    }
    let p = Project::new("redis", "giong-redis = \"1.5\"\n");
    let (s, r) = p.giong(&["db", "up"]);
    assert_eq!(s, 0, "{r}");
    assert!(p
        .env("REDIS_URL")
        .unwrap()
        .starts_with("redis://127.0.0.1:"));
    // Migrations are for SQL databases.
    let (s, r) = p.giong(&["db", "migrate"]);
    assert_eq!(s, 2, "{r}");
    // The container is named after this checkout, and without
    // .giong/db.json its password is unknown: refused, not guessed.
    let state = std::fs::read_to_string(p.dir.join(".giong/db.json")).unwrap();
    let name = format!("giong-giongclitest-redis-{}-redis-", std::process::id());
    assert!(state.contains(&name), "{state}");
    // Recorded for giong dev status and stop.
    let procs = std::fs::read_to_string(p.dir.join(".giong/processes.json")).unwrap();
    assert!(
        procs.contains(&name) && procs.contains("\"giong db up\""),
        "{procs}"
    );
    std::fs::remove_file(p.dir.join(".giong/db.json")).unwrap();
    let (s, r) = p.giong(&["db", "up"]);
    assert_ne!(s, 0, "{r}");
    assert_eq!(r["error"]["code"].as_str(), Some("GIONG-E5049"), "{r}");
    // down finds it by its label; up then starts a fresh one.
    let (s, r) = p.giong(&["db", "down", "--volumes"]);
    assert_eq!(s, 0, "{r}");
    let procs = std::fs::read_to_string(p.dir.join(".giong/processes.json")).unwrap();
    assert!(!procs.contains(&name), "{procs}");
    let (s, r) = p.giong(&["db", "up"]);
    assert_eq!(s, 0, "{r}");
}

#[test]
fn usage_mistakes_are_coded() {
    let p = Project::new("usage", "");
    let (s, r) = p.giong(&["db"]);
    assert_eq!((s, r["error"]["code"].as_str()), (2, Some("GIONG-E5000")));
    let (s, r) = p.giong(&["db", "explode"]);
    assert_eq!((s, r["error"]["code"].as_str()), (2, Some("GIONG-E5000")));
    // No database crate and no --engine.
    let (s, r) = p.giong(&["db", "up"]);
    assert_eq!((s, r["error"]["code"].as_str()), (2, Some("GIONG-E5000")));
    let (s, r) = p.giong(&["db", "up", "--engine", "oracle"]);
    assert_eq!((s, r["error"]["code"].as_str()), (2, Some("GIONG-E5000")));
    // Nothing started and no URL.
    let (s, r) = p.giong(&["db", "migrate"]);
    assert_eq!((s, r["error"]["code"].as_str()), (2, Some("GIONG-E5042")));
    let (s, r) = p.giong(&["db", "reset", "--yes"]);
    assert_eq!((s, r["error"]["code"].as_str()), (2, Some("GIONG-E5048")));
    // A bad file name.
    p.write("migrations/schema.sql", "x");
    let (s, r) = p.giong(&["db", "new", "x"]);
    assert_eq!((s, r["error"]["code"].as_str()), (2, Some("GIONG-E5045")));
}

#[test]
fn mysql_migrates_and_reports_the_failing_line() {
    if skip() {
        return;
    }
    let p = Project::new("my", "giong-mysql = \"1.5\"\n");
    let (s, r) = p.giong(&["db", "up"]);
    assert_eq!(s, 0, "{r}");
    assert!(p.env("DATABASE_URL").unwrap().starts_with("mysql://giong:"));
    p.write(
        "migrations/0001_items.sql",
        "CREATE TABLE items (id BIGINT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100));\n\
         INSERT INTO items (name) VALUES ('a;b');\n",
    );
    let (s, r) = p.giong(&["db", "migrate"]);
    assert_eq!(s, 0, "{r}");
    p.write(
        "migrations/0002_bad.sql",
        "CREATE TABLE t2 (x int);\nSELEC 1;\n",
    );
    let (s, r) = p.giong(&["db", "migrate"]);
    assert_eq!(s, 1, "{r}");
    assert_eq!(r["findings"][0]["code"], "GIONG-E5044");
    assert_eq!(r["findings"][0]["line"], 2, "{r}");
    // MySQL committed the CREATE TABLE: the fix must say so.
    assert!(r["findings"][0]["fix"].as_str().unwrap().contains("MySQL"));
    p.write(
        "migrations/0002_bad.sql",
        "CREATE TABLE IF NOT EXISTS t2 (x int);\n",
    );
    let (s, r) = p.giong(&["db", "migrate"]);
    assert_eq!(s, 0, "{r}");
    let (s, r) = p.giong(&["db", "reset", "--yes"]);
    assert_eq!(s, 0, "{r}");
    assert_eq!(r["data"]["applied"].as_array().unwrap().len(), 2, "{r}");
}

#[test]
fn a_second_project_gets_a_free_port() {
    if skip() {
        return;
    }
    let a = Project::new("porta", "giong-redis = \"1.5\"\n");
    let b = Project::new("portb", "giong-redis = \"1.5\"\n");
    let (s, r) = a.giong(&["db", "up"]);
    assert_eq!(s, 0, "{r}");
    let (s, r) = b.giong(&["db", "up"]);
    assert_eq!(s, 0, "{r}");
    assert_ne!(a.env("REDIS_URL"), b.env("REDIS_URL"));
    // An explicit port that is taken is a coded error.
    let taken = a.env("REDIS_URL").unwrap();
    let port = taken.rsplit(':').next().unwrap();
    let c = Project::new("portc", "giong-redis = \"1.5\"\n");
    let (s, r) = c.giong(&["db", "up", "--port", port]);
    assert_eq!(
        (s, r["error"]["code"].as_str()),
        (2, Some("GIONG-E5014")),
        "{r}"
    );
}

/// A `.giong/db.json` a cloned repository committed: its fields would name
/// the program run, the volume mounted and the containers removed. Every
/// entry giong db up could not have written is ignored with a warning.
#[test]
fn a_planted_state_file_is_ignored() {
    let p = Project::new("planted", "giong-redis = \"1.5\"\n");
    let marker = p.dir.join("ran");
    p.write(
        "x.sh",
        &format!("#!/bin/sh\ntouch {}\necho running\n", marker.display()),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p.dir.join("x.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    let decoy = format!("giongclitest-decoy-{}", std::process::id());
    let planted = |runtime: &str, volume: &str| {
        serde_json::json!({"redis": {
            "runtime": runtime, "container": decoy, "volume": volume,
            "image": "redis:7-alpine", "port": 6399, "user": "", "password": "",
            "database": "x; DROP DATABASE y",
        }})
        .to_string()
    };
    p.write(".giong/db.json", &planted("./x.sh", "/"));
    // status (a read-only MCP tool) runs nothing the file names.
    let (s, r) = p.giong(&["db", "status"]);
    assert_eq!(s, 0, "{r}");
    assert!(
        finding_codes(&r).contains(&"GIONG-E5048".to_string()),
        "{r}"
    );
    assert!(!exists(&marker), "status ran the planted runtime");
    // reset finds nothing giong db up started.
    let (s, r) = p.giong(&["db", "reset", "--yes"]);
    assert_eq!(r["error"]["code"].as_str(), Some("GIONG-E5048"), "{r}");
    assert_ne!(s, 0);
    assert!(!exists(&marker), "reset ran the planted runtime");
    if skip() {
        return;
    }
    // down leaves alone a container and a volume the file names but this
    // checkout does not own.
    let docker = runtime().unwrap();
    let made = Command::new(&docker)
        .args(["create", "--name", &decoy, "redis:7-alpine"])
        .output()
        .unwrap();
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    let _ = Command::new(&docker)
        .args(["volume", "create", &format!("{decoy}-data")])
        .output();
    p.write(
        ".giong/db.json",
        &planted(&docker, &format!("{decoy}-data")),
    );
    let (s, r) = p.giong(&["db", "down", "--volumes"]);
    assert_eq!(s, 0, "{r}");
    let alive = Command::new(&docker)
        .args(["inspect", &decoy])
        .output()
        .unwrap()
        .status
        .success();
    let volume_alive = Command::new(&docker)
        .args(["volume", "inspect", &format!("{decoy}-data")])
        .output()
        .unwrap()
        .status
        .success();
    let _ = Command::new(&docker).args(["rm", "-f", &decoy]).output();
    let _ = Command::new(&docker)
        .args(["volume", "rm", "-f", &format!("{decoy}-data")])
        .output();
    assert!(alive, "giong db down removed a container it does not own");
    assert!(
        volume_alive,
        "giong db down removed a volume it does not own"
    );
    // up starts this checkout's own container and volume, never the host
    // path the file names, and rewrites the entry.
    p.write(".giong/db.json", &planted(&docker, "/"));
    let (s, r) = p.giong(&["db", "up"]);
    assert_eq!(s, 0, "{r}");
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(p.dir.join(".giong/db.json")).unwrap())
            .unwrap();
    let container = state["redis"]["container"].as_str().unwrap().to_string();
    assert!(
        container.starts_with("giong-giongclitest-planted-"),
        "{state}"
    );
    assert_eq!(
        state["redis"]["volume"].as_str(),
        Some(format!("{container}-data").as_str()),
        "{state}"
    );
    let mounts = Command::new(&docker)
        .args([
            "inspect",
            "--format",
            "{{range .Mounts}}{{.Source}} {{end}}",
            &container,
        ])
        .output()
        .unwrap();
    let mounts = String::from_utf8_lossy(&mounts.stdout).to_string();
    assert!(!mounts.split_whitespace().any(|m| m == "/"), "{mounts}");
}
