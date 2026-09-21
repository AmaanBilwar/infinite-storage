use aws_config;
use aws_sdk_s3 as s3;
use clap::Parser;
use dotenv::dotenv;
use tokio;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}
#[derive(clap::Subcommand)]
enum Commands {
    ListBuckets,
    CreateBucket { bucket_name: String },
}

#[tokio::main]
async fn main() -> Result<(), s3::Error> {
    dotenv().ok();
    let cli = Cli::parse();
    let config = aws_config::load_from_env().await;
    let client = aws_sdk_s3::Client::new(&config);
    // this needs to be abstracted away eventually
    match cli.command {
        Commands::ListBuckets => {
            let output = client.list_buckets().send().await?;
            for bucket in output.buckets() {
                if let Some(name) = bucket.name() {
                    println!("{name}")
                }
            }
        }
        // create bucket
        Commands::CreateBucket { bucket_name } => {
            let response = client.create_bucket().send().await?;
            println!("{bucket_name}")
        }
    }

    Ok(())
}
