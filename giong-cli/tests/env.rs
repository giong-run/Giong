//! `giong env` on real files: init, show with masking, check's errors and
//! warnings, get's precedence, and the usage mistakes.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(tag: &str) -> Project {
        let dir = std::env::temp_dir().join(format!("giongclienv-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        Project { dir }
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.dir.join(name), text).unwrap();
    }

    fn giong(&self, args: &[&str], env: &[(&str, &str)]) -> (i32, Value) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_giong"));
        cmd.args(args).arg("--json").current_dir(&self.dir);
        for k in [
            "PORT",
            "DATABASE_URL",
            "API_TOKEN",
            "EXTRA",
            "SECRET",
            "CLOUD_ID",
        ] {
            cmd.env_remove(k);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let v = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
        (out.status.code().unwrap_or(-1), v)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn key<'a>(v: &'a Value, k: &str) -> &'a Value {
    v["data"]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["key"] == k)
        .unwrap_or_else(|| panic!("no {k} in {v}"))
}

fn codes(v: &Value, severity: &str) -> Vec<String> {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["severity"] == severity)
        .map(|f| f["title"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn init_then_check_then_fill_in() {
    let p = Project::new("init");
    let (code, v) = p.giong(&["env", "init"], &[]);
    assert_eq!(code, 2, "{v}");
    assert_eq!(v["error"]["code"], "GIONG-E5016");

    p.write(
        ".env.example",
        "# the port\nPORT=8080\nDATABASE_URL=\nAPI_TOKEN=\n",
    );
    let (code, v) = p.giong(&["env", "init"], &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["data"]["written"], true);
    let gitignore = std::fs::read_to_string(p.dir.join(".gitignore")).unwrap();
    assert!(gitignore.lines().any(|l| l == ".env"), "{gitignore}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(p.dir.join(".env"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // A second init keeps what is there.
    p.write(".env", "PORT=9000\n");
    let (_, v) = p.giong(&["env", "init"], &[]);
    assert_eq!(v["data"]["written"], false);
    assert_eq!(
        std::fs::read_to_string(p.dir.join(".env")).unwrap(),
        "PORT=9000\n"
    );

    let (code, v) = p.giong(&["env", "check"], &[]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(
        codes(&v, "error"),
        ["DATABASE_URL has no value", "API_TOKEN has no value"]
    );
    // The finding points at the line of .env.example that lists the key.
    let f = &v["findings"][0];
    assert_eq!(
        (f["file"].as_str(), f["line"].as_u64()),
        (Some(".env.example"), Some(3))
    );

    let (code, v) = p.giong(
        &["env", "check"],
        &[("DATABASE_URL", "postgres://h/db"), ("API_TOKEN", "t")],
    );
    assert_eq!(code, 0, "{v}");
    assert!(codes(&v, "error").is_empty());
}

#[test]
fn show_masks_secrets_and_names_sources() {
    let p = Project::new("show");
    p.write(".env.example", "PORT=\nDATABASE_URL=\nAPI_TOKEN=\n");
    p.write(
        ".env",
        "PORT=1\nPORT=2\nDATABASE_URL=postgres://app:hunter2@h:5432/d\nAPI_TOKEN='abc def'\nEXTRA=x\n",
    );
    let (code, v) = p.giong(&["env"], &[("PORT", "7")]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(key(&v, "PORT")["source"], "environment");
    assert_eq!(key(&v, "PORT")["value"], "7");
    assert_eq!(
        key(&v, "DATABASE_URL")["value"],
        "postgres://app:***@h:5432/d"
    );
    assert_eq!(key(&v, "API_TOKEN")["value"], "*** (set)");
    assert_eq!(key(&v, "API_TOKEN")["secret"], true);
    assert_eq!(key(&v, "EXTRA")["in_example"], false);
    assert!(!v.to_string().contains("hunter2"));
    assert!(!v.to_string().contains("abc def"));

    let (_, v) = p.giong(&["env", "--reveal"], &[]);
    assert_eq!(key(&v, "PORT")["value"], "2", "the last .env line wins");
    assert_eq!(key(&v, "API_TOKEN")["value"], "abc def");

    // A committed .env.example can list any key: a value only the process
    // environment holds is shown as set, whatever its name, unless revealed.
    p.write(
        ".env.example",
        "PORT=\nDATABASE_URL=\nAPI_TOKEN=\nCLOUD_ID=\n",
    );
    let (_, v) = p.giong(&["env"], &[("CLOUD_ID", "abc123")]);
    assert_eq!(key(&v, "CLOUD_ID")["source"], "environment");
    assert_eq!(key(&v, "CLOUD_ID")["value"], "*** (set in the environment)");
    assert!(!v.to_string().contains("abc123"));
    let (_, v) = p.giong(&["env", "--reveal"], &[("CLOUD_ID", "abc123")]);
    assert_eq!(key(&v, "CLOUD_ID")["value"], "abc123");
    p.write(".env.example", "PORT=\nDATABASE_URL=\nAPI_TOKEN=\n");

    let (code, v) = p.giong(&["env", "check"], &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(
        codes(&v, "warning"),
        ["EXTRA is in .env but not in .env.example"]
    );
}

#[test]
fn get_prefers_the_environment_and_fails_coded() {
    let p = Project::new("get");
    p.write(".env", "PORT=9000\nEMPTY=\n");
    let (code, v) = p.giong(&["env", "get", "PORT"], &[]);
    assert_eq!((code, &v["data"]["value"]), (0, &Value::from("9000")));
    let (_, v) = p.giong(&["env", "get", "PORT"], &[("PORT", "1")]);
    assert_eq!(v["data"]["value"], "1");
    for k in ["EMPTY", "NOPE"] {
        let (code, v) = p.giong(&["env", "get", k], &[]);
        assert_eq!(code, 2, "{v}");
        assert_eq!(v["error"]["code"], "GIONG-E5016");
    }
    // Plain output is the value alone, for $(giong env get PORT).
    let out = Command::new(env!("CARGO_BIN_EXE_giong"))
        .args(["env", "get", "PORT"])
        .env_remove("PORT")
        .current_dir(&p.dir)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "9000\n");
}

#[test]
fn bad_lines_and_usage_mistakes() {
    let p = Project::new("bad");
    p.write(".env.example", "PORT=8080\n");
    p.write(".env", "PORT=1\nnot a line\n1X=2\n");
    let (code, v) = p.giong(&["env", "check"], &[]);
    assert_eq!(code, 1, "{v}");
    let lines: Vec<u64> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["file"] == ".env")
        .map(|f| f["line"].as_u64().unwrap())
        .collect();
    assert_eq!(lines, [2, 3]);

    for args in [
        &["env", "frobnicate"][..],
        &["env", "show", "X"],
        &["env", "get"],
    ] {
        let (code, v) = p.giong(args, &[]);
        assert_eq!(code, 2, "{args:?}: {v}");
        assert_eq!(v["error"]["code"], "GIONG-E5000", "{args:?}");
    }
}

/// A cloned repository can commit `.env -> ~/.aws/credentials`: giong env
/// neither reads nor writes through it.
#[cfg(unix)]
#[test]
fn a_linked_dotenv_is_refused() {
    let p = Project::new("linked");
    let outside = p.dir.with_extension("outside");
    std::fs::write(&outside, "SECRET=hunter2\n").unwrap();
    p.write(".env.example", "SECRET=\n");
    std::os::unix::fs::symlink(&outside, p.dir.join(".env")).unwrap();
    for args in [&["env"][..], &["env", "init"], &["env", "get", "SECRET"]] {
        let (code, v) = p.giong(args, &[]);
        assert_eq!(code, 2, "{args:?}: {v}");
        assert!(!v.to_string().contains("hunter2"), "{v}");
    }
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "SECRET=hunter2\n"
    );
    std::fs::remove_file(&outside).unwrap();
}
