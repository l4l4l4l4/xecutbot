use anyhow::Result;
use clap::{Parser, Subcommand};
use xecut_bot::{backend::BackendImpl, selfupdate};

#[derive(Parser, Debug)]
struct Cli {
    #[arg(short = 'c', long = "config", default_value = "xecut_bot")]
    config: Vec<std::path::PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Render next Monday's source events without configuration, DB or Telegram.
    DecentralaPreview {
        /// Announcement date (YYYY-MM-DD), not the event date.
        #[arg(long)]
        date: chrono::NaiveDate,
        /// HTTP(S) CSV URL or local fixture path.
        #[arg(long, default_value = xecut_bot::decentrala::SOURCE_URL)]
        source: String,
        /// Editable .j2 override; absent uses the bundled template.
        #[arg(long)]
        template: Option<std::path::PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tokio::spawn(async move {
        env_logger::init();
        let args = Cli::parse();
        if let Some(Command::DecentralaPreview {
            date,
            source,
            template,
        }) = args.command
        {
            let date = xecut_bot::decentrala::next_monday(date)?;
            let source = xecut_bot::decentrala::load_source(&source).await?;
            let template = xecut_bot::decentrala::load_template(template.as_ref()).await?;
            let events = xecut_bot::decentrala::events_for_date(&source, date)?;
            if events.is_empty() {
                eprintln!("No Xecut events on {date}; nothing would be sent.");
            }
            for event in events {
                println!(
                    "{}",
                    xecut_bot::decentrala::render_caption(&event, &template)?
                );
                println!();
            }
            return Ok(());
        }
        let backend = BackendImpl::new(args.config).await?;
        let should_reexec = backend.run().await?;

        if should_reexec {
            selfupdate::reexec();
        }

        Ok(())
    })
    .await?
}
