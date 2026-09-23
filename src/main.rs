use aws_config;
use aws_sdk_s3 as s3;
use clap::Parser;
use dotenv::dotenv;
use tokio;
use serde::{Serialize, Deserialize};

const CONFIG_VERSION:u32 = 1;
const REGION: &str = "us-east-1";
const BUCKET_PREFIX: &str = "ise";

#[derive(Debug, Serialize, Deserialize)]
struct DriveConfig {
    id: String, 
    label: String,
    bucket: String,
    letter: char,
    active: bool,
}
#[derive(Debug, Serialize, Deserialize)]
struct DefaultSettings {
    region:String,
    bucket_prefix:String,
}

#[derive(Debug, Serialize, Deserialize)]
struct AppConfig {
    version: u32,
    defaults: DefaultSettings,
    drives: Vec<DriveConfig>,
}

#[derive(Parser)]
#[command(name = "ise")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}
#[derive(clap::Subcommand)]
enum Commands {
    Drive {
        #[command(subcommand)]
        command: DriveCommands,
    },
}

#[derive(clap::Subcommand)]
enum DriveCommands {
    List,
    Create { bucket_name: String },
    DeleteBucket { bucket_name: String },
    Delete { bucket_name:String, file_name : String },
    DeleteFiles { bucket_name:String, file_names : Vec<String> },
    Rename { bucket_name:String, old_file_name : String, new_file_name : String },
    Add {
        label: String,
        #[arg(long)]
        letter: Option<char>,
        #[arg(long)]
        bucket: Option<String>,
    },
    Update {
        label: String,
        #[arg(long)]
        letter: Option<char>,
        #[arg(long)]
        inactive: bool,
    },
    Remove { label: String },
    Sync {
        #[arg(long)]
        check: bool,
    },
}

// namethis function better
fn create_config(drives: Vec<DriveConfig>) -> AppConfig {
    AppConfig {
        version: CONFIG_VERSION,
        defaults: DefaultSettings {
            region: REGION.to_owned(),
            bucket_prefix: BUCKET_PREFIX.to_owned()
        },
        drives

    }
}

async fn client_builder(region: &str) -> s3::Client {
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_owned()))
        .load()
        .await;

    s3::Client::new(&config)
}

async fn list_buckets(client: &s3::Client) -> Result<(), s3::Error> {
    let output = client
        .list_buckets()
        .send()
        .await?;
    for bucket in output.buckets() {
        if let Some(name) = bucket.name() {
            println!("{name}")
        }
    }
    Ok(())
}

async fn create_bucket(client: &s3::Client, bucket_name:String) -> Result<(), s3::Error>{
    let full_name = format!("{BUCKET_PREFIX}-{bucket_name}");
    let _ = client
        .create_bucket()
        .bucket(&full_name)
        .send()
        .await?;
    println!("Created {full_name}");
    Ok(())
}

async fn delete_bucket(client: &s3::Client, bucket_name:String) -> Result<(), s3::Error>{
    // Versioning is off, so just empty live objects (paginated, single pass).
    let mut all_keys: Vec<String> = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let resp = client
            .list_objects_v2()
            .bucket(&bucket_name)
            .set_continuation_token(token.clone())
            .send()
            .await?;
        all_keys.extend(
            resp.contents()
                .iter()
                .filter_map(|obj| obj.key().map(|k| k.to_owned())),
        );
        if resp.is_truncated().unwrap_or(false) {
            token = resp.next_continuation_token().map(|s| s.to_owned());
        } else {
            break;
        }
    }

    if !all_keys.is_empty() {
        delete_files(client, bucket_name.clone(), all_keys).await?;
    }

    client.delete_bucket()
        .bucket(&bucket_name)
        .send().await?;
    println!("Deleted {bucket_name}");
    Ok(())
}

async fn delete_file(client: &s3::Client, bucket_name:String, file_name:String) -> Result<(), s3::Error>{
    let _ = client
        .delete_object()
        .bucket(&bucket_name)
        .key(&file_name)
        .send()
        .await?;

    println!("deleted {file_name}");
    Ok(())
}


async fn delete_files(client: &s3::Client, bucket_name:String, file_names:Vec<String>) -> Result<(), s3::Error>{
    // delete multiple files (S3 caps at 1000 keys per call, so chunk)
    for chunk in file_names.chunks(1000) {
        let objects = chunk
            .iter()
            .map(|file_name| {
                s3::types::ObjectIdentifier::builder()
                    .key(file_name)
                    .build()
                    .expect("an S3 object key is required")
            })
        .collect();

        let resp = client
            .delete_objects()
            .bucket(&bucket_name)
            .delete(
                s3::types::Delete::builder()
                .set_objects(Some(objects))
                .build()
                .expect("at least one S3 object is required"),
            )
            .send()
            .await?;

        println!("deleted {}", chunk.join(", "));
        for err in resp.errors() {
            eprintln!("failed {:?}: {:?} - {:?}", err.key(), err.code(), err.message());
        }
        eprintln!("deleted_ok={} errors={}", resp.deleted().len(), resp.errors().len());
    }
Ok(())
}

async fn rename_file(client: &s3::Client, bucket_name:String, old_file_name:String, new_file_name:String) -> Result<(), s3::Error>{
    let _ = client
        .rename_object()
        .bucket(&bucket_name)
        .rename_source(&old_file_name)
        .key(&new_file_name)
        .send()
        .await?;

    println!("renamed {old_file_name} to {new_file_name}!");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), s3::Error> {
    dotenv().ok();
    let cli = Cli::parse();
    let config = create_config(Vec::new());
    let client = client_builder(&config.defaults.region).await; 
    match cli.command {
        Commands::Drive { command } => match command {
            DriveCommands::List => {
                list_buckets(&client).await?;
            }
            DriveCommands::Create { bucket_name } => {
                create_bucket(&client, bucket_name).await?;
            }
            DriveCommands::DeleteBucket{ bucket_name } => {
                delete_bucket(&client, bucket_name).await?;
            }
            DriveCommands::Delete{ bucket_name, file_name } => {
                delete_file(&client, bucket_name, file_name).await?;
            }
            DriveCommands::DeleteFiles{ bucket_name, file_names } => {
                delete_files(&client, bucket_name, file_names).await?;
            }
            DriveCommands::Rename{ bucket_name, old_file_name, new_file_name } => {
                rename_file(&client, bucket_name, old_file_name, new_file_name ).await?;
            }
            DriveCommands::Add { .. }
            | DriveCommands::Update { .. }
            | DriveCommands::Remove { .. }
            | DriveCommands::Sync { .. } => {
                println!("Config drive commands are not implemented yet");
            }
        },
    }

    Ok(())
}
