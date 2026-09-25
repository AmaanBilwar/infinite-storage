use aws_config;
use aws_sdk_s3 as s3;
use clap::Parser;
use dotenv::dotenv;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio;

mod fs_mapping;

#[cfg(windows)]
mod winfsp_fs;

const CONFIG_VERSION: u32 = 1;
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
    region: String,
    bucket_prefix: String,
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
    Create {
        bucket_name: String,
    },
    DeleteBucket {
        bucket_name: String,
    },
    Delete {
        bucket_name: String,
        file_name: String,
    },
    DeleteFiles {
        bucket_name: String,
        file_names: Vec<String>,
    },
    Rename {
        bucket_name: String,
        old_file_name: String,
        new_file_name: String,
    },
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
    Remove {
        label: String,
    },
    Mount {
        label: String,
    },
    Sync {
        #[arg(long)]
        check: bool,
    },
}

fn config_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("ise").join("ise-config.json"))
}

// namethis function better
fn create_config(drives: Vec<DriveConfig>) -> AppConfig {
    AppConfig {
        version: CONFIG_VERSION,
        defaults: DefaultSettings {
            region: REGION.to_owned(),
            bucket_prefix: BUCKET_PREFIX.to_owned(),
        },
        drives,
    }
}

fn load_config() -> AppConfig {
    if let Some(path) = config_file_path()
        && let Ok(data) = std::fs::read_to_string(&path)
        && let Ok(cfg) = serde_json::from_str(&data)
    {
        return cfg;
    }
    create_config(Vec::new())
}

fn save_config(config: &AppConfig) -> std::io::Result<()> {
    let path = config_file_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no config dir"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, data)
}

async fn client_builder(region: &str) -> s3::Client {
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_owned()))
        .load()
        .await;

    s3::Client::new(&config)
}

async fn list_buckets(client: &s3::Client) -> Result<(), s3::Error> {
    let output = client.list_buckets().send().await?;
    for bucket in output.buckets() {
        if let Some(name) = bucket.name() {
            println!("{name}")
        }
    }
    Ok(())
}

async fn create_bucket(
    client: &s3::Client,
    config: &mut AppConfig,
    bucket_name: String,
) -> Result<(), s3::Error> {
    let full_name = format!("{BUCKET_PREFIX}-{bucket_name}");
    let _ = client.create_bucket().bucket(&full_name).send().await?;
    println!("Created {full_name}");
    if !config.drives.iter().any(|d| d.bucket == full_name) {
        config.drives.push(DriveConfig {
            id: full_name.clone(),
            label: bucket_name,
            bucket: full_name,
            letter: 'D',
            active: true,
        });
    }
    if let Err(e) = save_config(config) {
        eprintln!("warning: could not save config: {e}");
    }
    Ok(())
}

async fn delete_bucket(
    client: &s3::Client,
    config: &mut AppConfig,
    bucket_name: String,
) -> Result<(), s3::Error> {
    let full_name = if bucket_name.starts_with(&format!("{BUCKET_PREFIX}-")) {
        bucket_name
    } else {
        format!("{BUCKET_PREFIX}-{bucket_name}")
    };
    // Versioning is off, so just empty live objects (paginated, single pass).
    let mut all_keys: Vec<String> = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let resp = client
            .list_objects_v2()
            .bucket(&full_name)
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
        delete_files(client, full_name.clone(), all_keys).await?;
    }

    client.delete_bucket().bucket(&full_name).send().await?;
    println!("Deleted {full_name}");

    config.drives.retain(|d| d.bucket != full_name);
    if let Err(e) = save_config(config) {
        eprintln!("warning: could not save config: {e}");
    }
    Ok(())
}

async fn delete_file(
    client: &s3::Client,
    bucket_name: String,
    file_name: String,
) -> Result<(), s3::Error> {
    let _ = client
        .delete_object()
        .bucket(&bucket_name)
        .key(&file_name)
        .send()
        .await?;

    println!("deleted {file_name}");
    Ok(())
}

async fn delete_files(
    client: &s3::Client,
    bucket_name: String,
    file_names: Vec<String>,
) -> Result<(), s3::Error> {
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
            eprintln!(
                "failed {:?}: {:?} - {:?}",
                err.key(),
                err.code(),
                err.message()
            );
        }
        eprintln!(
            "deleted_ok={} errors={}",
            resp.deleted().len(),
            resp.errors().len()
        );
    }
    Ok(())
}

async fn rename_file(
    client: &s3::Client,
    bucket_name: String,
    old_file_name: String,
    new_file_name: String,
) -> Result<(), s3::Error> {
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

#[cfg(windows)]
async fn mount_configured_drive(client: &s3::Client, config: &AppConfig, label: String) {
    let Some(drive) = config.drives.iter().find(|d| d.label == label) else {
        eprintln!("no drive configured with label '{label}'");
        return;
    };
    if !drive.active {
        eprintln!("drive '{label}' is not active");
        return;
    }
    let handle = tokio::runtime::Handle::current();
    if let Err(e) = winfsp_fs::run_mount(
        client.clone(),
        drive.label.clone(),
        drive.bucket.clone(),
        drive.letter,
        handle,
    )
    .await
    {
        eprintln!("{e}");
    }
}

#[cfg(not(windows))]
async fn mount_configured_drive(_client: &s3::Client, config: &AppConfig, label: String) {
    if !config.drives.iter().any(|d| d.label == label) {
        eprintln!("no drive configured with label '{label}'");
        return;
    }
    eprintln!("mount is only supported on Windows (WinFSP)");
}

#[tokio::main]
async fn main() -> Result<(), s3::Error> {
    dotenv().ok();
    let cli = Cli::parse();
    let mut config = load_config();
    let client = client_builder(&config.defaults.region).await;
    match cli.command {
        Commands::Drive { command } => match command {
            DriveCommands::List => {
                list_buckets(&client).await?;
            }
            DriveCommands::Create { bucket_name } => {
                create_bucket(&client, &mut config, bucket_name).await?;
            }
            DriveCommands::DeleteBucket { bucket_name } => {
                delete_bucket(&client, &mut config, bucket_name).await?;
            }
            DriveCommands::Delete {
                bucket_name,
                file_name,
            } => {
                delete_file(&client, bucket_name, file_name).await?;
            }
            DriveCommands::DeleteFiles {
                bucket_name,
                file_names,
            } => {
                delete_files(&client, bucket_name, file_names).await?;
            }
            DriveCommands::Rename {
                bucket_name,
                old_file_name,
                new_file_name,
            } => {
                rename_file(&client, bucket_name, old_file_name, new_file_name).await?;
            }
            DriveCommands::Mount { label } => {
                mount_configured_drive(&client, &config, label).await;
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
