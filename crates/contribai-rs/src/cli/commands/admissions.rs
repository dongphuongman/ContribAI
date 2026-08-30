//! Inspect the local append-only admission decision ledger.

use anyhow::{bail, Context, Result};
use colored::Colorize;
use serde::Serialize;

use crate::cli::{create_memory, load_config};
use contribai::core::admission::{
    AdmissionAuditDecision, AdmissionAuditRecord, AdmissionAuditVerification,
};

#[derive(Serialize)]
struct AdmissionAuditOutput {
    schema_version: u8,
    chain: AdmissionAuditVerification,
    records: Vec<AdmissionAuditRecord>,
}

/// List and verify locally recorded admission decisions. This command performs no network access.
pub fn run_admissions(
    config_path: Option<&str>,
    repository: Option<&str>,
    decision: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<()> {
    if !(1..=1000).contains(&limit) {
        bail!("admission audit limit must be between 1 and 1000");
    }
    let normalized_decision = decision.map(|value| value.trim().to_ascii_lowercase());
    if let Some(value) = normalized_decision.as_deref() {
        if !AdmissionAuditDecision::is_valid_filter(value) {
            bail!(
                "invalid admission decision {value:?}; expected approved, blocked, rejected, skipped, or error"
            );
        }
    }
    let normalized_repository = repository.map(str::trim).filter(|value| !value.is_empty());
    let config = load_config(config_path)?;
    let memory = create_memory(&config)?;
    let chain = memory
        .verify_admission_audit_chain()
        .context("verifying the admission audit chain")?;
    let records = memory
        .get_admission_audits(normalized_repository, normalized_decision.as_deref(), limit)
        .context("reading the admission audit ledger")?;
    let output = AdmissionAuditOutput {
        schema_version: 1,
        chain,
        records,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_pretty(
            &output,
            normalized_repository,
            normalized_decision.as_deref(),
        );
    }

    if !output.chain.valid {
        bail!("local admission audit chain verification failed");
    }
    Ok(())
}

fn print_pretty(output: &AdmissionAuditOutput, repository: Option<&str>, decision: Option<&str>) {
    println!("{}", "ContribAI Admission Audit".cyan().bold());
    println!("{}", "━".repeat(68).dimmed());
    let chain_status = if output.chain.valid {
        "VALID".green().bold()
    } else {
        "INVALID".red().bold()
    };
    println!(
        "  Chain: {} ({} records checked)",
        chain_status, output.chain.records_checked
    );
    if let Some(value) = repository {
        println!("  Repository filter: {}", value.cyan());
    }
    if let Some(value) = decision {
        println!("  Decision filter: {}", value.cyan());
    }
    println!();

    if output.records.is_empty() {
        println!(
            "  {}",
            "No admission decisions match the current filter.".dimmed()
        );
        return;
    }

    for record in &output.records {
        let decision = match record.decision {
            AdmissionAuditDecision::Approved => record.decision.as_str().green().bold(),
            AdmissionAuditDecision::Blocked => record.decision.as_str().yellow().bold(),
            AdmissionAuditDecision::Rejected | AdmissionAuditDecision::Error => {
                record.decision.as_str().red().bold()
            }
            AdmissionAuditDecision::Skipped => record.decision.as_str().dimmed().bold(),
        };
        println!(
            "  {}  {}  {}  [{}]",
            record.recorded_at.format("%Y-%m-%d %H:%M"),
            record.repository.cyan(),
            record.stage.as_str().dimmed(),
            decision
        );
        println!(
            "    Receipt {}  Scope {} files / {} changed lines",
            short_receipt(&record.receipt).dimmed(),
            record.file_count,
            record.changed_lines
        );
        if let Some(permit) = &record.permit_id {
            println!("    Permit  {}", permit.dimmed());
        }
        println!("    {}", record.reason);
        println!();
    }
}

fn short_receipt(receipt: &str) -> &str {
    receipt.get(..12).unwrap_or(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_filter_is_strict_and_case_normalizable() {
        assert!(AdmissionAuditDecision::is_valid_filter("blocked"));
        assert!(!AdmissionAuditDecision::is_valid_filter("allow"));
        assert_eq!("APPROVED".to_ascii_lowercase(), "approved");
    }

    #[test]
    fn receipt_preview_is_safe_for_short_values() {
        assert_eq!(short_receipt("abcdef"), "abcdef");
        assert_eq!(short_receipt("0123456789abcdef"), "0123456789ab");
    }
}
