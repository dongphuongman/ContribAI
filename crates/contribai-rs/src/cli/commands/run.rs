//! Handles `Commands::Run` — discover and assess contribution candidates.

use colored::Colorize;

use crate::cli::{
    create_github, create_llm, create_memory, load_config, print_banner, print_config_summary,
    print_result,
};

pub async fn run_run(
    config_path: Option<&str>,
    language: Option<String>,
    stars: Option<String>,
    dry_run: bool,
    submit: bool,
    approve: bool,
) -> anyhow::Result<()> {
    print_banner();
    let mut config = load_config(config_path)?;
    let effective_dry_run = dry_run || !submit;
    let effective_mode = if submit { "build" } else { "plan" };

    config.pipeline.agent_mode = effective_mode.to_string();

    print_config_summary(&config, effective_dry_run);

    if let Some(lang) = &language {
        println!("   {}: {}", "Language".dimmed(), lang.cyan());
    }
    if let Some(s) = &stars {
        println!("   {}: {}", "Stars".dimmed(), s.cyan());
    }
    if approve {
        println!(
            "   {}: {}",
            "Approve".dimmed(),
            "HIGH risk enabled".yellow()
        );
    }
    println!(
        "   {}: {}",
        "Mode".dimmed(),
        if effective_mode == "plan" {
            "plan (read-only analysis)".yellow().to_string()
        } else {
            "build (full PR flow)".green().to_string()
        }
    );
    if !submit {
        println!(
            "   {}: {}",
            "Submission".dimmed(),
            "disabled (pass --submit for admitted draft PRs)".yellow()
        );
    }
    println!();

    let github = create_github(&config)?;
    let llm = create_llm(&config)?;
    let memory = create_memory(&config)?;
    let event_bus = contribai::core::events::EventBus::default();

    // ── v5.4: JSONL event logger ─────────────────────────────────
    let log_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".contribai")
        .join("events.jsonl");
    let _log_handle =
        contribai::core::events::FileEventLogger::new(&log_path).spawn_logger(&event_bus);
    println!("   {}: {}", "Event log".dimmed(), log_path.display());

    let mut pipeline = contribai::orchestrator::pipeline::ContribPipeline::new(
        &config,
        &github,
        llm.as_ref(),
        &memory,
        &event_bus,
    );
    pipeline.set_approve_high_risk(approve);
    pipeline.enable_external_writes(submit);

    let result = pipeline.run(None, effective_dry_run).await?;
    print_result(&result, effective_dry_run);
    Ok(())
}
