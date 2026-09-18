//! `contribai conformance` — deterministic conformance checks against the
//! safety core. Runs entirely offline; no network, no writes.
//!
//! Each check asserts a documented safety property: command classification,
//! protected-path policy, consent parsing, run-transition guards, and
//! fingerprint binding. A failing check means a safety invariant regressed.

use chrono::{Duration, Utc};
use std::collections::HashMap;

use contribai::core::admission::{
    is_dependency_manifest, is_full_commit_sha, is_protected_path, is_test_path,
    AdmissionController, AdmissionViolation, ContributionPermit, RepositoryConsent,
};
use contribai::core::command_safety::{classify_command, CommandClass};
use contribai::core::models::{
    Contribution, ContributionType, FileChange, Finding, Repository, Severity,
};
use contribai::core::run::{ContributionRun, RunState, RunTransitionError};

struct Check {
    name: &'static str,
    passed: bool,
    detail: String,
}

fn check(name: &'static str, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        name,
        passed,
        detail: detail.into(),
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

fn expect_class(parts: &[&str], want: CommandClass) -> Check {
    let verdict = classify_command(&argv(parts));
    check(
        "command_safety",
        verdict.class == want,
        format!(
            "{:?} → {} (want {}): {}",
            parts,
            verdict.class.as_str(),
            want.as_str(),
            verdict.reason
        ),
    )
}

const MANIFEST_V1: &str =
    "enabled: true\nmax_files: 5\nmax_changed_lines: 200\nallowed_paths:\n  - src/**\n";
const MANIFEST_V2: &str = "schema_version: 2\nenabled: true\nmax_files: 5\nmax_changed_lines: 200\nallowed_paths:\n  - src/**\ndenied_paths:\n  - src/secret/**\nallow_dependency_changes: false\nrequired_checks:\n  - cargo_test\n";
const MANIFEST_V2_UNKNOWN: &str = "schema_version: 2\nenabled: true\nunknown_field: oops\n";
const MANIFEST_V3: &str = "schema_version: 3\nenabled: true\n";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn fixture_repo() -> Repository {
    Repository {
        owner: "o".into(),
        name: "r".into(),
        full_name: "o/r".into(),
        description: None,
        language: None,
        languages: HashMap::new(),
        stars: 0,
        forks: 0,
        open_issues: 0,
        topics: vec![],
        default_branch: "main".into(),
        html_url: String::new(),
        clone_url: String::new(),
        has_contributing: false,
        has_license: true,
        last_push_at: None,
        created_at: None,
    }
}

fn fixture_permit() -> ContributionPermit {
    let consent =
        RepositoryConsent::parse(".contribai.yaml", MANIFEST_V2).expect("fixture manifest parses");
    ContributionPermit::issue(&fixture_repo(), SHA, consent, Some(1))
}

fn fixture_contribution(paths: &[(&str, &str, bool, bool)]) -> Contribution {
    let changes = paths
        .iter()
        .map(|(path, content, is_new, is_deleted)| FileChange {
            path: (*path).to_string(),
            original_content: None,
            new_content: (*content).to_string(),
            is_new_file: *is_new,
            is_deleted: *is_deleted,
        })
        .collect();
    Contribution {
        finding: Finding {
            id: "f".into(),
            finding_type: ContributionType::CodeQuality,
            severity: Severity::Medium,
            title: "t".into(),
            description: "d".into(),
            file_path: "src/a.rs".into(),
            line_start: None,
            line_end: None,
            suggestion: None,
            confidence: 0.8,
            priority_signals: vec![],
        },
        contribution_type: ContributionType::CodeQuality,
        title: "t".into(),
        description: "d".into(),
        changes,
        commit_message: "m".into(),
        tests_added: vec![],
        branch_name: String::new(),
        generated_at: Utc::now(),
    }
}

fn expect_violation(name: &'static str, paths: &[(&str, &str, bool, bool)], want: &str) -> Check {
    let report = AdmissionController::evaluate(
        &fixture_repo(),
        &fixture_contribution(paths),
        &fixture_permit(),
        Utc::now(),
    );
    let hit = report.violations.iter().any(|v| violation_label(v) == want);
    check(
        name,
        hit && !report.allowed,
        format!(
            "{paths:?} → {} (want {want})",
            violation_labels(&report.violations)
        ),
    )
}

fn violation_label(v: &AdmissionViolation) -> &'static str {
    match v {
        AdmissionViolation::UnsupportedDeletion { .. } => "unsupported_deletion",
        AdmissionViolation::ProtectedPath { .. } => "protected_path",
        AdmissionViolation::InvalidPath { .. } => "invalid_path",
        AdmissionViolation::DeniedPath { .. } => "denied_path",
        AdmissionViolation::PathOutsidePermit { .. } => "path_outside_permit",
        AdmissionViolation::DependencyChangeNotAllowed { .. } => "dependency_change",
        AdmissionViolation::NewFilesNotAllowed { .. } => "new_files",
        AdmissionViolation::TestChangeNotAllowed { .. } => "test_changes",
        AdmissionViolation::TooManyFiles { .. } => "too_many_files",
        AdmissionViolation::TooManyChangedLines { .. } => "too_many_lines",
        _ => "other",
    }
}

fn violation_labels(violations: &[AdmissionViolation]) -> String {
    violations
        .iter()
        .map(violation_label)
        .collect::<Vec<_>>()
        .join(",")
}

fn run_checks() -> Vec<Check> {
    // Expression-only checks first; procedural run-guard checks push after.
    let mut out = vec![
        // ── Command safety ────────────────────────────────────────────
        expect_class(&["cargo", "test", "--workspace"], CommandClass::Safe),
        expect_class(&["npm", "test"], CommandClass::Safe),
        // Regression: `go test ./...` must not read as a path escape.
        expect_class(&["go", "test", "./..."], CommandClass::Safe),
        expect_class(&["curl", "https://x"], CommandClass::Forbidden),
        // Metachar argv refuses shell reinterpretation — never silently Safe.
        check(
            "command_safety",
            classify_command(&argv(&["echo", "x", "|", "sh"])).class != CommandClass::Safe,
            "argv containing '|' is never Safe",
        ),
        expect_class(&["rm", "-rf", "/"], CommandClass::Forbidden),
        expect_class(&["npm", "publish"], CommandClass::Forbidden),
        expect_class(&["git", "push"], CommandClass::Forbidden),
        expect_class(&["node", "-e", "x"], CommandClass::Forbidden),
        expect_class(&["python", "-c", "x"], CommandClass::Forbidden),
        expect_class(&["aws", "s3", "sync"], CommandClass::Forbidden),
        expect_class(
            &["some_unrecognized_tool", "--version"],
            CommandClass::RequiresApproval,
        ),
        // Regression: `~` in `HEAD~1` must not read as a shell metachar.
        check(
            "command_safety",
            classify_command(&argv(&["git", "diff", "HEAD~1"])).class != CommandClass::Forbidden,
            "git diff HEAD~1 is not forbidden",
        ),
        // ── Path classifiers ──────────────────────────────────────────
        check(
            "protected_path",
            is_protected_path(".github/workflows/ci.yml"),
            ".github/workflows protected",
        ),
        check(
            "protected_path",
            is_protected_path("LICENSE"),
            "LICENSE protected",
        ),
        check(
            "protected_path",
            !is_protected_path("src/main.rs"),
            "src/main.rs not protected",
        ),
        check(
            "test_path",
            is_test_path("tests/foo_test.rs"),
            "tests/ classified as test",
        ),
        check(
            "dep_manifest",
            is_dependency_manifest("Cargo.toml"),
            "Cargo.toml is a dependency manifest",
        ),
        check(
            "lockfile",
            contribai::core::admission::is_lockfile("package-lock.json"),
            "package-lock.json is a lockfile",
        ),
        // ── Consent parsing ───────────────────────────────────────────
        check(
            "consent_v1",
            RepositoryConsent::parse(".contribai.yaml", MANIFEST_V1)
                .as_ref()
                .is_some_and(|c| c.schema_version == 1),
            "schema 1 manifest parses",
        ),
        check(
            "consent_v2",
            RepositoryConsent::parse(".contribai.yaml", MANIFEST_V2)
                .as_ref()
                .is_some_and(|c| {
                    c.schema_version == 2
                        && c.denied_paths.len() == 1
                        && !c.allow_dependency_changes
                        && c.required_checks == ["cargo_test"]
                }),
            "schema 2 manifest parses with bounded policy",
        ),
        check(
            "consent_fail_closed",
            RepositoryConsent::parse(".contribai.yaml", MANIFEST_V2_UNKNOWN).is_none(),
            "unknown field rejected fail-closed",
        ),
        check(
            "consent_fail_closed",
            RepositoryConsent::parse(".contribai.yaml", MANIFEST_V3).is_none(),
            "unsupported schema version rejected",
        ),
        {
            let label_consent = RepositoryConsent::from_label(7, "contribai-approved");
            check(
                "consent_label",
                label_consent.schema_version == 1 && label_consent.draft_only,
                "label consent constructs with conservative defaults",
            )
        },
        // ── Base revision ─────────────────────────────────────────────
        check("base_sha", is_full_commit_sha(SHA), "40-hex accepted"),
        check(
            "base_sha",
            !is_full_commit_sha("main"),
            "branch ref rejected",
        ),
        check(
            "base_sha",
            !is_full_commit_sha(&SHA[..12]),
            "short SHA rejected",
        ),
        // ── Admission evaluation (adversarial paths) ──────────────────
        expect_violation(
            "admission",
            &[("src/a.rs", "fn a() {}\n", false, true)],
            "unsupported_deletion",
        ),
        expect_violation(
            "admission",
            &[(".github/workflows/ci.yml", "on: push\n", false, false)],
            "protected_path",
        ),
        expect_violation(
            "admission",
            &[("src/..\\escape.rs", "x", false, false)],
            "invalid_path",
        ),
        expect_violation(
            "admission",
            &[("../outside.rs", "x", false, false)],
            "invalid_path",
        ),
        expect_violation(
            "admission",
            &[("src/secret/key.rs", "x", false, false)],
            "denied_path",
        ),
        expect_violation(
            "admission",
            &[("docs/readme.md", "x", false, false)],
            "path_outside_permit",
        ),
        expect_violation(
            "admission",
            &[("src/Cargo.toml", "[package]\n", false, false)],
            "dependency_change",
        ),
        // A clean in-scope change must admit — fail-open would hide bugs.
        check(
            "admission",
            {
                let report = AdmissionController::evaluate(
                    &fixture_repo(),
                    &fixture_contribution(&[("src/a.rs", "fn a() {}\n", false, false)]),
                    &fixture_permit(),
                    Utc::now(),
                );
                report.allowed && report.violations.is_empty()
            },
            "in-scope change admits cleanly",
        ),
        // ── Run transition guards ─────────────────────────────────────
        check(
            "run_transition",
            RunState::Discovered.can_transition_to(RunState::Authorized),
            "discovered → authorized allowed",
        ),
        check(
            "run_transition",
            !RunState::Submitted.can_transition_to(RunState::Approved),
            "terminal states never transition",
        ),
    ];

    // authorize without a permit → MissingPermitBinding
    let mut run = ContributionRun::new("o/r", Some(1), SHA, Utc::now() + Duration::hours(2));
    let err = run
        .transition(RunState::Authorized, "authorize", "x", Utc::now())
        .unwrap_err();
    out.push(check(
        "run_guard",
        err == RunTransitionError::MissingPermitBinding,
        "authorize without permit denied",
    ));

    // invalid forward jump → InvalidTransition
    let mut run2 = ContributionRun::new("o/r", Some(1), SHA, Utc::now() + Duration::hours(2));
    let err2 = run2
        .transition(RunState::Validating, "jump", "x", Utc::now())
        .unwrap_err();
    out.push(check(
        "run_guard",
        matches!(err2, RunTransitionError::InvalidTransition { .. }),
        "discovered → validating denied",
    ));

    // ReadyForReview without candidate → MissingCandidate (walk a legal path first)
    let mut run3 = ContributionRun::new("o/r", Some(1), SHA, Utc::now() + Duration::hours(2));
    run3.permit_id = Some("p".into());
    run3.transition(RunState::Authorized, "authorize", "x", Utc::now())
        .unwrap();
    run3.transition(RunState::Prepared, "prepare", "x", Utc::now())
        .unwrap();
    run3.transition(RunState::Planned, "plan", "x", Utc::now())
        .unwrap();
    run3.transition(RunState::Executing, "execute", "x", Utc::now())
        .unwrap();
    run3.transition(RunState::Validating, "validate", "x", Utc::now())
        .unwrap();
    run3.transition(RunState::Challenging, "challenge", "x", Utc::now())
        .unwrap();
    let err3 = run3
        .transition(RunState::ReadyForReview, "evidence", "x", Utc::now())
        .unwrap_err();
    out.push(check(
        "run_guard",
        err3 == RunTransitionError::MissingCandidate,
        "ready_for_review without candidate denied",
    ));

    // Submitted with mismatched review fingerprint → ReviewBindingMismatch
    let mut run4 = ContributionRun::new("o/r", Some(1), SHA, Utc::now() + Duration::hours(2));
    run4.permit_id = Some("p".into());
    run4.candidate_fingerprint = Some("cand".into());
    run4.review_fingerprint = Some("other".into());
    for (to, ev) in [
        (RunState::Authorized, "authorize"),
        (RunState::Prepared, "prepare"),
        (RunState::Planned, "plan"),
        (RunState::Executing, "execute"),
        (RunState::Validating, "validate"),
        (RunState::Challenging, "challenge"),
        (RunState::ReadyForReview, "evidence"),
        (RunState::Approved, "approve"),
    ] {
        run4.transition(to, ev, "x", Utc::now()).unwrap();
    }
    let err4 = run4
        .transition(RunState::Submitted, "submit", "x", Utc::now())
        .unwrap_err();
    out.push(check(
        "run_guard",
        err4 == RunTransitionError::ReviewBindingMismatch,
        "submit with substituted review fingerprint denied",
    ));

    // Approved → Submitted succeeds when fingerprints match
    let mut run5 = ContributionRun::new("o/r", Some(1), SHA, Utc::now() + Duration::hours(2));
    run5.permit_id = Some("p".into());
    run5.candidate_fingerprint = Some("cand".into());
    run5.review_fingerprint = Some("cand".into());
    for (to, ev) in [
        (RunState::Authorized, "authorize"),
        (RunState::Prepared, "prepare"),
        (RunState::Planned, "plan"),
        (RunState::Executing, "execute"),
        (RunState::Validating, "validate"),
        (RunState::Challenging, "challenge"),
        (RunState::ReadyForReview, "evidence"),
        (RunState::Approved, "approve"),
    ] {
        run5.transition(to, ev, "x", Utc::now()).unwrap();
    }
    out.push(check(
        "run_guard",
        run5.transition(RunState::Submitted, "submit", "x", Utc::now())
            .is_ok(),
        "approved → submitted allowed with matching fingerprints",
    ));

    // Run id format: run_ + 24 hex
    out.push(check(
        "run_id",
        run.run_id.starts_with("run_") && run.run_id.len() == 28,
        format!("run id format {}", run.run_id),
    ));

    out
}

/// `contribai conformance` — offline deterministic conformance report.
pub fn run_conformance(json: bool) -> anyhow::Result<()> {
    let checks = run_checks();
    let failed = checks.iter().filter(|c| !c.passed).count();

    if json {
        let payload = serde_json::json!({
            "total": checks.len(),
            "passed": checks.len() - failed,
            "failed": failed,
            "ok": failed == 0,
            "checks": checks.iter().map(|c| serde_json::json!({
                "name": c.name,
                "passed": c.passed,
                "detail": c.detail,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        for c in &checks {
            println!(
                "  [{}] {}",
                if c.passed { "pass" } else { "FAIL" },
                c.detail
            );
        }
        println!(
            "\n{} checks, {} passed, {} failed",
            checks.len(),
            checks.len() - failed,
            failed
        );
    }
    if failed > 0 {
        anyhow::bail!("{failed} conformance check(s) failed");
    }
    Ok(())
}
