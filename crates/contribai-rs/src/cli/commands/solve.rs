//! Handles `Commands::Solve` through the shared admission-gated pipeline.

use colored::Colorize;

use crate::cli::{
    create_github, create_llm, create_memory, load_config, parse_github_url, print_banner,
    print_result,
};

pub async fn run_solve(
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
        "Solving approved issues in: {} {}",
        url.cyan().bold(),
        if effective_dry_run {
            "(READ-ONLY)".yellow().to_string()
        } else {
            "(ADMITTED DRAFT SUBMISSION)".green().to_string()
        }
    );
    if !submit {
        println!("  Pass --submit to request an admission-gated draft PR.");
    }
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
        .solve_targeted(
            &owner,
            &name,
            effective_dry_run,
            config.github.max_prs_per_day as usize,
        )
        .await?;
    print_result(&result, effective_dry_run);
    Ok(())
}
