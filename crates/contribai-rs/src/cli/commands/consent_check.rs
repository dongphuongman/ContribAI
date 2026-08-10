//! Inspect repository-side ContribAI consent without invoking an LLM or writing to GitHub.

use colored::Colorize;
use serde::Serialize;

use crate::cli::{create_github, load_config, parse_github_url, print_banner};
use contribai::core::admission::{discover_repository_consent, RepositoryConsent, CONSENT_PATHS};

#[derive(Debug, Serialize)]
struct ConsentCheckReport {
    schema_version: u8,
    repository: String,
    consent_found: bool,
    repository_gate_ready: bool,
    checked_paths: Vec<&'static str>,
    base_sha: Option<String>,
    consent: Option<RepositoryConsent>,
    reason: Option<&'static str>,
}

impl ConsentCheckReport {
    fn build(
        repository: String,
        consent: Option<RepositoryConsent>,
        base_sha: Option<String>,
    ) -> Self {
        let reason = if consent.is_none() {
            Some("no valid repository consent manifest was found")
        } else if base_sha.is_none() {
            Some("the default branch revision could not be attested")
        } else {
            None
        };
        Self {
            schema_version: 1,
            repository,
            consent_found: consent.is_some(),
            repository_gate_ready: consent.is_some() && base_sha.is_some(),
            checked_paths: CONSENT_PATHS.to_vec(),
            base_sha,
            consent,
            reason,
        }
    }
}

pub async fn run_consent_check(
    config_path: Option<&str>,
    url: String,
    json: bool,
    require_consent: bool,
) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let (owner, name) = parse_github_url(&url)?;
    let github = create_github(&config)?;
    let repository = github.get_repo_details(&owner, &name).await?;
    let consent = discover_repository_consent(&github, &owner, &name).await;
    let base_sha = github
        .get_branch_info(&owner, &name, &repository.default_branch)
        .await
        .ok()
        .and_then(|branch| branch["commit"]["sha"].as_str().map(str::to_string))
        .filter(|sha| !sha.is_empty());
    let report = ConsentCheckReport::build(repository.full_name, consent, base_sha);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_banner();
        println!("Repository: {}", report.repository.cyan().bold());
        println!(
            "Consent:    {}",
            if report.consent_found {
                "FOUND".green().bold().to_string()
            } else {
                "NOT FOUND".yellow().bold().to_string()
            }
        );
        if let Some(consent) = &report.consent {
            println!("Source:     {:?}", consent.source);
            println!(
                "Budget:     {} files / {} changed lines",
                consent.max_files, consent.max_changed_lines
            );
            println!(
                "Paths:      {}",
                if consent.allowed_paths.is_empty() {
                    "all non-protected code paths".to_string()
                } else {
                    consent.allowed_paths.join(", ")
                }
            );
        } else {
            println!("Checked:    {}", report.checked_paths.join(", "));
        }
        println!(
            "Base SHA:   {}",
            report.base_sha.as_deref().unwrap_or("unavailable")
        );
        println!(
            "Gate:       {}",
            if report.repository_gate_ready {
                "READY FOR CANDIDATE GENERATION".green().to_string()
            } else {
                "CLOSED".yellow().to_string()
            }
        );
        println!();
        println!(
            "This check performs no write and does not approve a patch; evidence and human review remain mandatory."
        );
    }

    if require_consent && !report.repository_gate_ready {
        anyhow::bail!(report.reason.unwrap_or("repository consent gate is closed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use contribai::core::admission::ConsentSource;

    fn consent() -> RepositoryConsent {
        RepositoryConsent {
            source: ConsentSource::RepositoryManifest {
                path: ".github/contribai.yml".to_string(),
            },
            max_files: 3,
            max_changed_lines: 120,
            allowed_paths: vec!["src/**".to_string()],
            draft_only: true,
        }
    }

    #[test]
    fn report_is_ready_only_with_consent_and_base_revision() {
        assert!(
            ConsentCheckReport::build(
                "owner/repo".to_string(),
                Some(consent()),
                Some("abc123".to_string())
            )
            .repository_gate_ready
        );
        assert!(
            !ConsentCheckReport::build("owner/repo".to_string(), None, None).repository_gate_ready
        );
    }

    #[test]
    fn report_json_is_stable_and_machine_readable() {
        let report = ConsentCheckReport::build(
            "owner/repo".to_string(),
            Some(consent()),
            Some("abc123".to_string()),
        );
        let value = serde_json::to_value(report).expect("serializable report");
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["repository_gate_ready"], true);
        assert_eq!(value["consent"]["max_files"], 3);
    }
}
