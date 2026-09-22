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
    DeleteBucket { bucket_name: String },
    DeleteFile { bucket_name:String, file_name : String },
    DeleteFiles { bucket_name:String, file_names : Vec<String> },
    RenameFile { bucket_name:String, old_file_name : String, new_file_name : String },
    // upload one file
    // upload multiple files
}


async fn client_builder() -> s3::Client {
    let config = aws_config::load_from_env().await;
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
    let _ = client
        .create_bucket()
        .bucket(&bucket_name)
        .send()
        .await?;
    println!("Created {bucket_name}");
    Ok(())
}

async fn delete_bucket(client: &s3::Client, bucket_name:String) -> Result<(), s3::Error>{
    let _ = client
        .delete_bucket()
        .bucket(&bucket_name)
        .send()
        .await?;
    println!("Created {bucket_name}");
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
    // delete multiple files
    // ai wrote this idk whats happening 
    let objects = file_names
        .iter()
        .map(|file_name| {
            s3::types::ObjectIdentifier::builder()
                .key(file_name)
                .build()
                .expect("an S3 object key is required")
        })
    .collect();

    client
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

    println!("deleted {}", file_names.join(", "));
Ok(())
}

async fn rename_file(client: &s3::Client, bucket_name:String, old_file_name:String, new_file_name:String) -> Result<(), s3::Error>{
    let _ =   client
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
    let client = client_builder().await; 
    match cli.command {
        Commands::ListBuckets => {
            list_buckets(&client).await?;
        }
        // create bucket
        Commands::CreateBucket { bucket_name } => {
            create_bucket(&client, bucket_name).await?;
        }
        // delete a bucket
        Commands::DeleteBucket{ bucket_name } => {
            delete_bucket(&client, bucket_name).await?;
        }
         // delete one file 
        Commands::DeleteFile{ bucket_name, file_name } => {
            delete_file(&client, bucket_name, file_name).await?;
        }
        // delete multiples files
        Commands::DeleteFiles{ bucket_name, file_names } => {
            delete_files(&client, bucket_name, file_names).await?;
        }
        // rename a file
        Commands::RenameFile{ bucket_name, old_file_name, new_file_name } => {
            rename_file(&client, bucket_name, old_file_name, new_file_name ).await?;
}
}

    Ok(())
}
