//! Offline, deterministic walkthrough of ContribAI's admission boundary.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use colored::Colorize;
use serde::Serialize;

use contribai::core::admission::{
    AdmissionController, ContributionPermit, EvidenceCapsule, EvidenceCheck, RepositoryConsent,
};
use contribai::core::models::{
    Contribution, ContributionType, FileChange, Finding, Repository, Severity,
};

const DEMO_REPOSITORY: &str = "demo/hello-contribai";
const DEMO_BASE_SHA: &str = "7a93c4d1f802b6e5c2f54968bd42676b751faf09";
const DEMO_MANIFEST: &str = r#"schema_version: 1
enabled: true
max_files: 2
max_changed_lines: 40
allowed_paths:
  - src/**
  - tests/**
"#;

#[derive(Debug, Serialize)]
struct DemoReport {
    schema_version: u8,
    mode: &'static str,
    repository: String,
    base_sha: String,
    consent: RepositoryConsent,
    candidate: CandidateResult,
    protected_path_probe: PolicyProbe,
    human_review: &'static str,
    submission: &'static str,
    external_writes_enabled: bool,
}

#[derive(Debug, Serialize)]
struct CandidateResult {
    title: String,
    admission_allowed: bool,
    violations: Vec<String>,
    file_count: usize,
    changed_lines: usize,
    paths: Vec<String>,
    evidence_valid: bool,
    evidence: EvidenceSummary,
}

#[derive(Debug, Serialize)]
struct EvidenceSummary {
    schema_version: u8,
    permit_id: String,
    contribution_fingerprint: String,
    draft_only: bool,
    checks: Vec<EvidenceCheck>,
}

#[derive(Debug, Serialize)]
struct PolicyProbe {
    path: String,
    admission_allowed: bool,
    violations: Vec<String>,
}

/// Run the no-network safety walkthrough.
pub fn run_demo(json: bool, manifest_path: Option<&Path>) -> Result<()> {
    let manifest = if let Some(path) = manifest_path {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read demo manifest {}", path.display()))?
    } else {
        DEMO_MANIFEST.to_string()
    };
    let report = build_demo_report(&manifest)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!();
    println!("{}", "ContribAI offline safety demo".bold());
    println!("Mode:             offline / read-only");
    println!("Network access:   none");
    println!("External writes:  {}", "DISABLED".green().bold());
    println!();
    println!(
        "{} Parsed maintainer consent: {} files / {} changed lines",
        "01  CONSENT".green().bold(),
        report.consent.max_files,
        report.consent.max_changed_lines
    );
    println!(
        "{} Bound permit to {} @ {}…",
        "02  BIND".green().bold(),
        report.repository,
        &report.base_sha[..12]
    );
    println!(
        "{} Candidate admitted: {} files / {} changed lines",
        "03  VERIFY".green().bold(),
        report.candidate.file_count,
        report.candidate.changed_lines
    );
    println!(
        "{} Evidence v{} validates for the exact candidate",
        "04  EVIDENCE".green().bold(),
        report.candidate.evidence.schema_version
    );
    println!(
        "{} Protected-path probe denied: {}",
        "05  FAIL CLOSED".yellow().bold(),
        report.protected_path_probe.path
    );
    println!(
        "{} Human review is still required",
        "06  REVIEW".yellow().bold()
    );
    println!(
        "{} No branch or pull request was created",
        "07  STOP".green().bold()
    );
    println!();
    println!(
        "{}",
        "PASS — policy exercised; submission capability remained off."
            .green()
            .bold()
    );
    println!("Inspect the machine-readable receipt with: contribai demo --json");

    Ok(())
}

fn build_demo_report(manifest: &str) -> Result<DemoReport> {
    let repository = demo_repository();
    let consent = RepositoryConsent::parse(".github/contribai.yml", manifest)
        .context("demo consent manifest is invalid or disabled")?;
    let permit = ContributionPermit::issue(&repository, DEMO_BASE_SHA, consent.clone(), None);
    let candidate = demo_candidate();
    let admission = AdmissionController::evaluate(&repository, &candidate, &permit, Utc::now());
    let evidence = EvidenceCapsule::build(&candidate, &permit, &admission, Vec::new());
    let evidence_valid = evidence
        .validate_for_submission(&candidate, &repository, Utc::now())
        .is_ok();

    let protected_candidate = protected_path_candidate();
    let protected_report =
        AdmissionController::evaluate(&repository, &protected_candidate, &permit, Utc::now());

    Ok(DemoReport {
        schema_version: 1,
        mode: "offline_read_only",
        repository: repository.full_name,
        base_sha: permit.base_sha,
        consent,
        candidate: CandidateResult {
            title: candidate.title,
            admission_allowed: admission.allowed,
            violations: admission
                .violations
                .iter()
                .map(ToString::to_string)
                .collect(),
            file_count: admission.file_count,
            changed_lines: admission.changed_lines,
            paths: admission.paths,
            evidence_valid,
            evidence: EvidenceSummary {
                schema_version: evidence.schema_version,
                permit_id: evidence.permit_id,
                contribution_fingerprint: evidence.contribution_fingerprint,
                draft_only: evidence.draft_only,
                checks: evidence.checks,
            },
        },
        protected_path_probe: PolicyProbe {
            path: protected_report.paths.first().cloned().unwrap_or_default(),
            admission_allowed: protected_report.allowed,
            violations: protected_report
                .violations
                .iter()
                .map(ToString::to_string)
                .collect(),
        },
        human_review: "required",
        submission: "not_attempted",
        external_writes_enabled: false,
    })
}

fn demo_repository() -> Repository {
    Repository {
        owner: "demo".to_string(),
        name: "hello-contribai".to_string(),
        full_name: DEMO_REPOSITORY.to_string(),
        description: Some("Bundled ContribAI safety walkthrough".to_string()),
        language: Some("JavaScript".to_string()),
        languages: HashMap::new(),
        stars: 0,
        forks: 0,
        open_issues: 0,
        topics: vec!["contribai-demo".to_string()],
        default_branch: "main".to_string(),
        html_url: String::new(),
        clone_url: String::new(),
        has_contributing: true,
        has_license: true,
        last_push_at: None,
        created_at: None,
    }
}

fn demo_finding() -> Finding {
    Finding {
        id: "demo-input-normalization".to_string(),
        finding_type: ContributionType::CodeQuality,
        severity: Severity::Low,
        title: "Normalize blank greeting names".to_string(),
        description: "Keep the greeting deterministic for blank input.".to_string(),
        file_path: "src/greeting.js".to_string(),
        line_start: Some(1),
        line_end: Some(3),
        suggestion: Some("Trim the supplied name and use a documented fallback.".to_string()),
        confidence: 1.0,
        priority_signals: vec!["bundled_fixture".to_string()],
    }
}

fn demo_candidate() -> Contribution {
    Contribution {
        finding: demo_finding(),
        contribution_type: ContributionType::CodeQuality,
        title: "fix: normalize blank greeting names".to_string(),
        description: "Use a deterministic fallback and cover it with a focused test.".to_string(),
        changes: vec![FileChange {
            path: "src/greeting.js".to_string(),
            original_content: Some(
                "export function greet(name) {\n  return \"Hello, \" + name + \"!\";\n}\n"
                    .to_string(),
            ),
            new_content: "export function greet(name = \"maintainer\") {\n  const safeName = name.trim() || \"maintainer\";\n  return \"Hello, \" + safeName + \"!\";\n}\n".to_string(),
            is_new_file: false,
            is_deleted: false,
        }],
        commit_message: "fix: normalize blank greeting names".to_string(),
        tests_added: vec![FileChange {
            path: "tests/greeting.test.js".to_string(),
            original_content: None,
            new_content: "import assert from \"node:assert/strict\";\nimport { greet } from \"../src/greeting.js\";\n\nassert.equal(greet(\"\"), \"Hello, maintainer!\");\n".to_string(),
            is_new_file: true,
            is_deleted: false,
        }],
        branch_name: "contribai/normalize-greeting".to_string(),
        generated_at: Utc::now(),
    }
}

fn protected_path_candidate() -> Contribution {
    let mut candidate = demo_candidate();
    candidate.title = "chore: modify release workflow".to_string();
    candidate.changes = vec![FileChange {
        path: ".github/workflows/release.yml".to_string(),
        original_content: Some("permissions:\n  contents: read\n".to_string()),
        new_content: "permissions:\n  contents: write\n".to_string(),
        is_new_file: false,
        is_deleted: false,
    }];
    candidate.tests_added.clear();
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_exercises_real_policy_without_enabling_writes() {
        let report = build_demo_report(DEMO_MANIFEST).expect("demo report");

        assert_eq!(report.mode, "offline_read_only");
        assert!(report.candidate.admission_allowed);
        assert!(report.candidate.evidence_valid);
        assert!(!report.protected_path_probe.admission_allowed);
        assert!(report
            .protected_path_probe
            .violations
            .iter()
            .any(|violation| violation.contains("protected path")));
        assert_eq!(report.human_review, "required");
        assert_eq!(report.submission, "not_attempted");
        assert!(!report.external_writes_enabled);
    }

    #[test]
    fn demo_json_has_a_stable_top_level_contract() {
        let value = serde_json::to_value(build_demo_report(DEMO_MANIFEST).expect("demo report"))
            .expect("serialize demo report");

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["repository"], DEMO_REPOSITORY);
        assert_eq!(value["candidate"]["evidence"]["schema_version"], 2);
        assert_eq!(value["protected_path_probe"]["admission_allowed"], false);
        assert_eq!(value["external_writes_enabled"], false);
    }

    #[test]
    fn local_demo_manifest_fails_closed_when_invalid() {
        let error = build_demo_report("schema_version: 1\nenabled: false\n")
            .expect_err("disabled consent must fail");

        assert!(error
            .to_string()
            .contains("demo consent manifest is invalid or disabled"));
    }
}
