use crate::ssh::SshConnection;
use std::{fs::Permissions, io::Write, os::unix::fs::PermissionsExt};

pub(crate) async fn rsync(ssh: &SshConnection, append_args: Vec<String>) {
    let mut known_hosts_file = tempfile::NamedTempFile::new().unwrap();
    known_hosts_file
        .write_all(ssh.openssh_known_hosts_line().as_bytes())
        .unwrap();
    let known_hosts_path = known_hosts_file.path();

    let mut key_file = tempfile::NamedTempFile::new().unwrap();
    key_file
        .write_all(ssh.client_private_key.as_bytes())
        .unwrap();
    let key_path = key_file.path();
    std::fs::set_permissions(key_path, Permissions::from_mode(0o400)).unwrap();

    let mut args = vec![
        "--delete".to_owned(),
        "-e".to_owned(),
        format!(
            "ssh -i {} -o 'UserKnownHostsFile {}'",
            key_path.display(),
            known_hosts_path.display()
        ),
        "-ra".to_owned(),
    ];
    args.extend(append_args);

    let output = tokio::process::Command::new("rsync")
        .args(args)
        .output()
        .await
        .unwrap();
    if !output.status.success() {
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        panic!("rsync failed:\nstdout:\n{stdout}\nstderr:\n{stderr}")
    }
}
