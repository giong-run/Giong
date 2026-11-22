//! `giong env`: what an agent sees of the configuration. The promise that
//! matters most is that a secret never reaches stdout unless `--reveal` asks
//! for it, because agents paste that output into logs and model context.

use crate::harness::*;

fn project(tag: &str, example: &str, dotenv: &str) -> TempDir {
    let dir = TempDir::new(tag);
    new_package(&dir, "envp", "");
    dir.write(".env.example", example);
    if !dotenv.is_empty() {
        dir.write(".env", dotenv);
    }
    dir
}

/// Secret values that must not appear on stdout without `--reveal`.
fn leaks<'a>(stdout: &str, secrets: &[&'a str]) -> Vec<&'a str> {
    secrets
        .iter()
        .copied()
        .filter(|s| stdout.contains(s))
        .collect()
}

#[test]
fn secrets_are_masked_in_text_and_json_and_revealed_only_on_request() {
    if !has_command("env") {
        return;
    }
    let dir = project(
        "env-mask",
        "DATABASE_URL=\nAPI_TOKEN=\nPORT=8080\n",
        "DATABASE_URL=postgres://u:pw-url-1@h/db\nAPI_TOKEN=\"pw-token-2\"\n\
         E2E_SECRET_ACCESS_KEY=pw-aws-3\nREDIS_URL=redis://:pw-redis-4@h:6379\nPORT=9000\n",
    );
    let secrets = ["pw-url-1", "pw-token-2", "pw-aws-3", "pw-redis-4"];
    for args in [&["env"][..], &["env", "show"], &["env", "check"]] {
        let json = giong(args).arg("--json").dir(dir.path()).run();
        let v = json.report("env");
        assert_eq!(
            leaks(&json.stdout, &secrets),
            Vec::<&str>::new(),
            "{}",
            json.dump()
        );
        let text = giong(args).dir(dir.path()).run();
        assert_eq!(
            text.code(),
            crate::schema::exit_code_for(&v),
            "text and --json disagree on the exit code"
        );
        assert_eq!(
            leaks(&text.stdout, &secrets),
            Vec::<&str>::new(),
            "{}",
            text.dump()
        );
        assert_eq!(
            leaks(&text.stderr, &secrets),
            Vec::<&str>::new(),
            "{}",
            text.dump()
        );
        // Non-secrets are shown as they are.
        assert!(json.stdout.contains("9000"), "{}", json.dump());
    }
    let shown = giong(["env", "--reveal", "--json"]).dir(dir.path()).run();
    shown.report("env");
    for s in secrets {
        assert!(
            shown.stdout.contains(s),
            "--reveal hides {s}\n{}",
            shown.dump()
        );
    }
}

/// Spellings of secrets common in real services. Each one here is a value
/// an agent would otherwise copy into its context.
#[test]
fn common_secret_spellings_are_masked_too() {
    if !has_command("env") {
        return;
    }
    let dir = project(
        "env-mask-more",
        "",
        "DB_PASS=pw-pass-1\nMYSQL_PWD=pw-pwd-2\nSTRIPE_SK=pw-sk-3\n\
         DSN=postgres://h/db?user=u&password=pw-query-4\n\
         SESSION_SIGNING_SALT=pw-salt-5\n",
    );
    let out = giong(["env", "--json"]).dir(dir.path()).run();
    out.report("env");
    let found = leaks(
        &out.stdout,
        &[
            "pw-pass-1",
            "pw-pwd-2",
            "pw-sk-3",
            "pw-query-4",
            "pw-salt-5",
        ],
    );
    assert!(
        found.is_empty(),
        "shown unmasked: {found:?}\n{}",
        out.dump()
    );
}

#[test]
fn a_line_that_cannot_be_read_is_reported_without_its_value() {
    if !has_command("env") {
        return;
    }
    let dir = project("env-badline", "API_TOKEN=\n", "API_TOKEN pw-line-1\n");
    let out = giong(["env", "check", "--json"]).dir(dir.path()).run();
    let v = out.report("env");
    assert_eq!(v["ok"], false, "{}", out.dump());
    assert!(!out.stdout.contains("pw-line-1"), "{}", out.dump());
    let text = giong(["env", "check"]).dir(dir.path()).run();
    assert!(!text.stdout.contains("pw-line-1") && !text.stderr.contains("pw-line-1"));
}

#[test]
fn check_errors_on_a_missing_key_and_warns_on_an_undocumented_one() {
    if !has_command("env") {
        return;
    }
    let dir = project("env-check", "NEEDED=\nPORT=8080\n", "PORT=1\nEXTRA=1\n");
    let out = giong(["env", "check", "--json"]).dir(dir.path()).run();
    let v = out.report("env");
    out.expect_code(1);
    let f = v["findings"].as_array().unwrap();
    let missing = f
        .iter()
        .find(|f| f["severity"] == "error" && f["title"].as_str().unwrap().contains("NEEDED"))
        .unwrap_or_else(|| panic!("no error for NEEDED\n{}", out.dump()));
    assert_eq!(missing["code"], "GIONG-E5016");
    // Points at the line of .env.example that lists it.
    assert_eq!(missing["file"], ".env.example");
    assert_eq!(missing["line"], 1);
    assert!(
        f.iter()
            .any(|f| f["severity"] == "warning" && f["title"].as_str().unwrap().contains("EXTRA")),
        "no warning for EXTRA\n{}",
        out.dump()
    );

    // The environment satisfies a key too; then only the warning stays.
    let out = giong(["env", "check", "--json"])
        .dir(dir.path())
        .env("NEEDED", "from-env")
        .run();
    let v = out.report("env");
    out.expect_code(0);
    assert_eq!(v["ok"], true);
}

#[test]
fn get_prefers_the_environment_and_fails_with_a_code_on_an_unknown_key() {
    if !has_command("env") {
        return;
    }
    let dir = project("env-get", "PORT=\n", "PORT=9000\n");
    let out = giong(["env", "get", "PORT"])
        .dir(dir.path())
        .env_remove("PORT")
        .run();
    out.expect_code(0);
    assert_eq!(out.stdout.trim_end(), "9000");
    let out = giong(["env", "get", "PORT"])
        .dir(dir.path())
        .env("PORT", "1")
        .run();
    assert_eq!(out.stdout.trim_end(), "1");
    let out = giong(["env", "get", "NOPE_E2E", "--json"])
        .dir(dir.path())
        .env_remove("NOPE_E2E")
        .run();
    let v = out.report("env");
    out.expect_code(2);
    assert_eq!(v["error"]["code"], "GIONG-E5016");
}

#[test]
fn init_writes_env_once_and_never_replaces_it() {
    if !has_command("env") {
        return;
    }
    let dir = project("env-init", "PORT=8080\nDATABASE_URL=\n", "");
    let out = giong(["env", "init", "--json"]).dir(dir.path()).run();
    out.report("env");
    assert!(dir.join(".env").exists(), "{}", out.dump());
    let first = dir.read(".env");
    assert!(first.contains("PORT=8080"), "{first}");

    dir.write(".env", "PORT=1\nDATABASE_URL=postgres://u:pw@h/db\n");
    let out = giong(["env", "init", "--json"]).dir(dir.path()).run();
    out.report("env");
    assert_eq!(
        dir.read(".env"),
        "PORT=1\nDATABASE_URL=postgres://u:pw@h/db\n",
        "init replaced an existing .env\n{}",
        out.dump()
    );
}

#[test]
fn odd_but_common_dotenv_spellings_are_read() {
    if !has_command("env") {
        return;
    }
    // CRLF, a BOM, `export`, quotes, an inline comment, spaces around `=`.
    let dir = project(
        "env-spellings",
        "A=\nB=\nC=\nD=\nE=\n",
        "\u{feff}A=1\r\nexport B=2\r\nC=\"three # not a comment\"\r\nD='4' # comment\r\nE = 5\r\n",
    );
    for (k, want) in [
        ("A", "1"),
        ("B", "2"),
        ("C", "three # not a comment"),
        ("D", "4"),
        ("E", "5"),
    ] {
        let out = giong(["env", "get", k]).dir(dir.path()).env_remove(k).run();
        assert_eq!(
            out.stdout.trim_end_matches(['\r', '\n']),
            want,
            "{k}\n{}",
            out.dump()
        );
    }
}

#[test]
fn outside_a_project_it_says_so() {
    if !has_command("env") {
        return;
    }
    let dir = TempDir::new("env-outside");
    let out = giong(["env", "check", "--json"]).dir(dir.path()).run();
    let v = out.report("env");
    // Either a coded failure, or a check of nothing; never a crash.
    assert!(matches!(out.code(), 0 | 2), "{}", out.dump());
    if out.code() == 2 {
        assert!(v["error"]["code"].is_string());
    }
}
