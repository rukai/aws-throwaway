use aws_throwaway::{Aws, CleanupResources, Ec2Instance, Ec2InstanceDefinition, InstanceType};
use std::{path::Path, time::Instant};
use tracing_subscriber::EnvFilter;

const FILE_LEN: usize = 1024 * 1024 * 100; // 100MB

#[tokio::main]
async fn main() {
    let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stdout());
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(non_blocking)
        .init();

    let aws = Aws::builder(CleanupResources::AllResources).build().await;
    let instance = aws
        .create_ec2_instance(Ec2InstanceDefinition::new(InstanceType::T2Micro))
        .await;

    let start = Instant::now();
    std::fs::write("some_local_file", vec![0; FILE_LEN]).unwrap(); // create 100MB file
    println!("Time to create 100MB file locally {:?}", start.elapsed());

    let start = Instant::now();
    instance
        .ssh()
        .push_file(Path::new("some_local_file"), Path::new("some_remote_file"))
        .await;
    println!("Time to push 100MB file via ssh {:?}", start.elapsed());
    assert_remote_size(&instance).await;

    let start = Instant::now();
    instance
        .ssh()
        .push_rsync(Path::new("some_local_file"), "some_remote_file")
        .await;
    println!("Time to push 100MB file via rsync {:?}", start.elapsed());
    assert_remote_size(&instance).await;

    let start = Instant::now();
    instance
        .ssh()
        .pull_file(Path::new("some_remote_file"), Path::new("some_local_file"))
        .await;
    println!("Time to pull 100MB file via ssh {:?}", start.elapsed());
    assert_local_size();

    let start = Instant::now();
    instance
        .ssh()
        .pull_rsync("some_remote_file", Path::new("some_local_file"))
        .await;
    println!("Time to pull 100MB file via rsync {:?}", start.elapsed());
    assert_local_size();

    aws.cleanup_resources().await;
    println!("\nAll AWS throwaway resources have been deleted")
}

async fn assert_remote_size(instance: &Ec2Instance) {
    let remote_size: usize = instance
        .ssh()
        .shell("wc -c some_remote_file")
        .await
        .stdout
        .split_ascii_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(remote_size, FILE_LEN);
}

fn assert_local_size() {
    assert_eq!(std::fs::read("some_local_file").unwrap().len(), FILE_LEN);
    std::fs::remove_file("some_local_file").unwrap();
}
