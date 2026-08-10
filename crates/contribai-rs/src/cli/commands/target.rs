//! Handles `Commands::Target` — target a specific repository.

use colored::Colorize;

use crate::cli::{
    create_github, create_llm, create_memory, load_config, parse_github_url, print_banner,
    print_result,
};

pub async fn run_target(
    config_path: Option<&str>,
    url: String,
    dry_run: bool,
    submit: bool,
) -> anyhow::Result<()> {
    print_banner();
    let mut config = load_config(config_path)?;
    let effective_dry_run = dry_run || !submit;
    config.pipeline.agent_mode = if submit { "build" } else { "plan" }.to_string();

    println!(
        "🎯 Targeting: {} {}",
        url.cyan().bold(),
        if effective_dry_run {
            "(DRY RUN)".yellow().to_string()
        } else {
            "(LIVE)".green().to_string()
        }
    );
    println!();

    let (owner, name) = parse_github_url(&url)?;

    let github = create_github(&config)?;
    let llm = create_llm(&config)?;
    let memory = create_memory(&config)?;
    let event_bus = contribai::core::events::EventBus::default();

    let mut pipeline = contribai::orchestrator::pipeline::ContribPipeline::new(
        &config,
        &github,
        llm.as_ref(),
        &memory,
        &event_bus,
    );
    pipeline.enable_external_writes(submit);

    let result = pipeline
        .run_targeted(&owner, &name, effective_dry_run)
        .await?;
    print_result(&result, effective_dry_run);
    Ok(())
}
