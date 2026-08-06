use std::env;
use std::io::{self, Read, Write};
use std::path::Path;

use git_rs::{
    HostFileSystem, Repository, UploadPackOptions, UploadPackRequest, UploadPackV2Limits,
    UploadPackV2Request,
};

fn main() -> git_rs::Result<()> {
    let repository_path = env::args()
        .nth(1)
        .expect("usage: upload_pack_service <repository>");
    let repository_path = std::fs::canonicalize(Path::new(&repository_path))?;
    let repository = Repository::open(
        HostFileSystem::new(repository_path.parent().expect("repository parent"))?,
        repository_path.file_name().expect("repository name"),
    )?;
    let mut stdout = io::stdout().lock();
    if env::var("GIT_PROTOCOL").is_ok_and(|value| value.split(':').any(|item| item == "version=2"))
    {
        stdout.write_all(&repository.advertise_upload_pack_v2()?)?;
        stdout.flush()?;
        let limits = UploadPackV2Limits::default();
        let mut stdin = io::stdin().lock();
        while let Some(input) = read_v2_request(&mut stdin)? {
            if input == b"0000" {
                break;
            }
            let request = UploadPackV2Request::parse(&input, &limits)?;
            stdout.write_all(&repository.respond_upload_pack_v2(
                &request,
                &UploadPackOptions::default(),
                &limits,
            )?)?;
            stdout.flush()?;
        }
        return Ok(());
    }
    stdout.write_all(&repository.advertise_upload_pack()?)?;
    stdout.flush()?;
    let mut input = Vec::new();
    let mut stdin = io::stdin().lock();
    let mut saw_want = false;
    loop {
        let mut header = [0; 4];
        if let Err(error) = stdin.read_exact(&mut header) {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                break;
            }
            return Err(error.into());
        }
        input.extend_from_slice(&header);
        let length = usize::from_str_radix(
            std::str::from_utf8(&header)
                .map_err(|_| git_rs::Error::Protocol("packet header is not ASCII".into()))?,
            16,
        )
        .map_err(|_| git_rs::Error::Protocol("invalid packet header".into()))?;
        if length == 0 {
            if !saw_want {
                break;
            }
            continue;
        }
        if length < 4 {
            return Err(git_rs::Error::Protocol("invalid control packet".into()));
        }
        let mut payload = vec![0; length - 4];
        stdin.read_exact(&mut payload)?;
        saw_want |= payload.starts_with(b"want ");
        let done = payload == b"done\n" || payload == b"done";
        input.extend_from_slice(&payload);
        if done {
            break;
        }
    }
    if !input.is_empty() {
        let request = UploadPackRequest::parse(&input)?;
        stdout
            .write_all(&repository.respond_upload_pack(&request, &UploadPackOptions::default())?)?;
    }
    Ok(())
}

fn read_v2_request(input: &mut impl Read) -> git_rs::Result<Option<Vec<u8>>> {
    let mut request = Vec::new();
    loop {
        let mut header = [0; 4];
        if let Err(error) = input.read_exact(&mut header) {
            return if error.kind() == io::ErrorKind::UnexpectedEof {
                Ok(None)
            } else {
                Err(error.into())
            };
        }
        request.extend_from_slice(&header);
        let length = usize::from_str_radix(
            std::str::from_utf8(&header)
                .map_err(|_| git_rs::Error::Protocol("packet header is not ASCII".into()))?,
            16,
        )
        .map_err(|_| git_rs::Error::Protocol("invalid packet header".into()))?;
        if length == 0 {
            return Ok(Some(request));
        }
        if length <= 2 {
            continue;
        }
        if length < 4 {
            return Err(git_rs::Error::Protocol("invalid packet length".into()));
        }
        let mut payload = vec![0; length - 4];
        input.read_exact(&mut payload)?;
        request.extend_from_slice(&payload);
    }
}
