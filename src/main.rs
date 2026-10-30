use clap::Parser;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    version,
    about = "Display symbolic link targets or resolve absolute paths"
)]
struct Args {
    #[arg(
        short = 'e',
        long,
        conflicts_with = "canonicalize",
        help = "Resolve absolute paths; every component must exist"
    )]
    canonicalize_existing: bool,

    #[arg(
        short = 'f',
        long,
        help = "Resolve absolute paths; only the final component may be missing"
    )]
    canonicalize: bool,

    #[arg(
        short = 'n',
        long,
        conflicts_with = "zero",
        help = "Omit output delimiters"
    )]
    no_newline: bool,

    #[arg(short = 'z', long, help = "Separate output with NUL bytes")]
    zero: bool,

    #[arg(required = true, value_name = "PATH", help = "Paths to process")]
    paths: Vec<PathBuf>,
}

fn components(path: &Path, existing: bool) -> VecDeque<OsString> {
    let mut parts: VecDeque<_> = path
        .components()
        .map(|part| part.as_os_str().to_owned())
        .collect();
    let bytes = path.as_os_str().as_encoded_bytes();
    let is_separator = |byte| byte == b'/' || (cfg!(windows) && byte == b'\\');
    let trailing_dot = bytes
        .rsplit(|&byte| is_separator(byte))
        .find(|part| !part.is_empty())
        == Some(b".");
    let trailing_separator = bytes.last().is_some_and(|&byte| is_separator(byte));
    if trailing_dot || (existing && trailing_separator) {
        parts.push_back(OsString::from("."));
    }
    parts
}

fn canonicalise(path: &Path, existing: bool) -> io::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty path"));
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut pending = components(&absolute, existing);
    let mut resolved = PathBuf::new();
    let mut links = 0;

    while let Some(part) = pending.pop_front() {
        match Path::new(&part).components().next() {
            Some(Component::Prefix(_) | Component::RootDir) => resolved.push(part),
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                resolved.pop();
            }
            Some(Component::Normal(name)) => {
                let candidate = resolved.join(name);
                match fs::symlink_metadata(&candidate) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        links += 1;
                        if links > 40 {
                            return Err(io::Error::other(
                                "symbolic link loop or too many symbolic links",
                            ));
                        }
                        let target = fs::read_link(&candidate)?;
                        for part in components(&target, existing).into_iter().rev() {
                            pending.push_front(part);
                        }
                    }
                    Ok(metadata) => {
                        if !pending.is_empty() && !metadata.is_dir() {
                            return Err(io::Error::new(
                                io::ErrorKind::NotADirectory,
                                format!("{}: not a directory", candidate.display()),
                            ));
                        }
                        resolved.push(name);
                    }
                    Err(error)
                        if !existing
                            && pending.is_empty()
                            && error.kind() == io::ErrorKind::NotFound =>
                    {
                        resolved.push(name);
                    }
                    Err(error) => return Err(error),
                }
            }
            None => {}
        }
    }

    Ok(resolved)
}

fn resolve(path: &Path, args: &Args) -> io::Result<PathBuf> {
    if args.canonicalize_existing || args.canonicalize {
        canonicalise(path, args.canonicalize_existing)
    } else if fs::symlink_metadata(path)?.file_type().is_symlink() {
        fs::read_link(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a symbolic link",
        ))
    }
}

fn run(args: &Args, output: &mut impl Write, errors: &mut impl Write) -> io::Result<bool> {
    let delimiter: &[u8] = if args.zero {
        b"\0"
    } else if args.no_newline {
        b""
    } else {
        b"\n"
    };
    let mut success = true;

    for path in &args.paths {
        match resolve(path, args) {
            Ok(target) => {
                output.write_all(target.as_os_str().as_encoded_bytes())?;
                output.write_all(delimiter)?;
            }
            Err(error) => {
                writeln!(errors, "readlink: {path:?}: {error}")?;
                success = false;
            }
        }
    }

    output.flush()?;
    Ok(success)
}

fn main() -> ExitCode {
    let args = Args::parse();
    let mut output = io::stdout().lock();
    let mut errors = io::stderr().lock();

    match run(&args, &mut output, &mut errors) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            let _ = writeln!(errors, "readlink: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::current_dir()
                .unwrap()
                .join("target")
                .join("readlink-tests");
            fs::create_dir_all(&root).unwrap();
            loop {
                let path = root.join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("cannot create fixture: {error}"),
                }
            }
        }

        fn path(&self, name: impl AsRef<Path>) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn rejects_missing_paths_and_conflicting_flags() {
        for arguments in [
            vec!["readlink"],
            vec!["readlink", "-n", "-z", "link"],
            vec!["readlink", "--zero", "--no-newline", "link"],
            vec!["readlink", "-e", "-f", "link"],
            vec!["readlink", "--unknown", "link"],
        ] {
            assert!(Args::try_parse_from(arguments).is_err());
        }
        let args = Args::try_parse_from(["readlink", "--", "-link", "other"]).unwrap();
        assert_eq!(args.paths, [PathBuf::from("-link"), PathBuf::from("other")]);
    }

    #[test]
    fn requires_existing_components_except_the_final_one() {
        let fixture = Fixture::new();
        let file = fixture.path("file");
        fs::write(&file, b"fixture\n").unwrap();
        let expected = std::path::absolute(&file).unwrap();
        assert_eq!(canonicalise(&file, true).unwrap(), expected);
        assert_eq!(canonicalise(&file, false).unwrap(), expected);

        let missing = fixture.path("missing");
        assert_eq!(canonicalise(&missing, false).unwrap(), missing);
        assert!(canonicalise(&missing, true).is_err());
        assert!(canonicalise(&missing.join("child"), false).is_err());
        assert!(canonicalise(&missing.join("../file"), false).is_err());
        assert!(canonicalise(&file.join("child"), false).is_err());
        assert!(canonicalise(&file.join(".."), false).is_err());
        assert!(canonicalise(&file.join("."), true).is_err());
        assert!(canonicalise(&missing.join("."), false).is_err());
        assert!(canonicalise(&missing.join("./"), false).is_err());
        assert!(canonicalise(Path::new(""), false).is_err());
    }

    #[test]
    fn writes_each_delimiter_and_continues_after_failures() {
        let fixture = Fixture::new();
        let first = fixture.path("first");
        let second = fixture.path("second");
        fs::write(&first, b"first\n").unwrap();
        fs::write(&second, b"second\n").unwrap();
        let missing = fixture.path("missing");

        for (flag, delimiter) in [
            (None, b"\n".as_slice()),
            (Some("-n"), b""),
            (Some("-z"), b"\0"),
        ] {
            let mut arguments = vec![OsString::from("readlink"), OsString::from("-e")];
            arguments.extend(flag.map(OsString::from));
            arguments.extend(
                [first.clone(), missing.clone(), second.clone()].map(PathBuf::into_os_string),
            );
            let args = Args::try_parse_from(arguments).unwrap();
            let mut output = Vec::new();
            let mut errors = Vec::new();
            assert!(!run(&args, &mut output, &mut errors).unwrap());
            let mut expected = Vec::new();
            for path in [&first, &second] {
                expected.extend_from_slice(path.as_os_str().as_encoded_bytes());
                expected.extend_from_slice(delimiter);
            }
            assert_eq!(output, expected);
            assert_eq!(String::from_utf8(errors).unwrap().lines().count(), 1);
        }
    }

    #[test]
    fn rejects_regular_files_in_default_mode() {
        let fixture = Fixture::new();
        let file = fixture.path("file");
        fs::write(&file, b"fixture\n").unwrap();
        let args =
            Args::try_parse_from([OsString::from("readlink"), file.into_os_string()]).unwrap();
        let mut output = Vec::new();
        let mut errors = Vec::new();
        assert!(!run(&args, &mut output, &mut errors).unwrap());
        assert!(output.is_empty());
        assert!(
            String::from_utf8(errors)
                .unwrap()
                .contains("not a symbolic link")
        );
    }

    #[test]
    fn reports_output_failures() {
        struct BrokenOutput;

        impl Write for BrokenOutput {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed pipe"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let args = Args::try_parse_from(["readlink", "-e", "."]).unwrap();
        assert_eq!(
            run(&args, &mut BrokenOutput, &mut Vec::new())
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolves_relative_links_before_parent_components() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        fs::create_dir_all(fixture.path("actual/nested")).unwrap();
        fs::create_dir(fixture.path("links")).unwrap();
        fs::write(fixture.path("actual/file"), b"fixture\n").unwrap();
        symlink("../actual/nested", fixture.path("links/alias")).unwrap();

        let args = Args::try_parse_from(["readlink", "link"]).unwrap();
        assert_eq!(
            resolve(&fixture.path("links/alias"), &args).unwrap(),
            PathBuf::from("../actual/nested")
        );
        for existing in [true, false] {
            assert_eq!(
                canonicalise(&fixture.path("links/alias/../file"), existing).unwrap(),
                fs::canonicalize(fixture.path("actual/file")).unwrap()
            );
        }
        symlink(fixture.path("actual/file"), fixture.path("absolute")).unwrap();
        assert_eq!(
            canonicalise(&fixture.path("absolute"), true).unwrap(),
            fs::canonicalize(fixture.path("actual/file")).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn follows_dangling_links_only_when_the_final_target_may_be_missing() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        fs::create_dir(fixture.path("links")).unwrap();
        symlink("../missing", fixture.path("links/dangling")).unwrap();
        symlink("dangling", fixture.path("links/chain")).unwrap();
        assert_eq!(
            canonicalise(&fixture.path("links/chain"), false).unwrap(),
            fixture.path("missing")
        );
        assert!(canonicalise(&fixture.path("links/chain"), true).is_err());
        assert!(canonicalise(&fixture.path("links/chain/child"), false).is_err());
        symlink("../absent/child", fixture.path("links/bad")).unwrap();
        assert!(canonicalise(&fixture.path("links/bad"), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn detects_symbolic_link_loops() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        symlink("second", fixture.path("first")).unwrap();
        symlink("first", fixture.path("second")).unwrap();
        for existing in [true, false] {
            let error = canonicalise(&fixture.path("first"), existing).unwrap_err();
            assert!(error.to_string().contains("symbolic link loop"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_arguments_and_output_bytes() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let target = OsString::from_vec(b"target-\xff".to_vec());
        let link = fixture.path(OsString::from_vec(b"link-\xfe".to_vec()));
        fs::write(fixture.path(&target), b"fixture\n").unwrap();
        symlink(&target, &link).unwrap();

        for canonical in [false, true] {
            let mut arguments = vec![OsString::from("readlink"), OsString::from("-z")];
            if canonical {
                arguments.push(OsString::from("-e"));
            }
            arguments.push(link.clone().into_os_string());
            let args = Args::try_parse_from(arguments).unwrap();
            let mut output = Vec::new();
            let mut errors = Vec::new();
            assert!(run(&args, &mut output, &mut errors).unwrap());
            let mut expected = if canonical {
                fs::canonicalize(fixture.path(&target))
                    .unwrap()
                    .into_os_string()
                    .into_vec()
            } else {
                target.clone().into_vec()
            };
            expected.push(0);
            assert_eq!(output, expected);
            assert!(errors.is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn enforces_directory_requirements_for_terminal_components() {
        let fixture = Fixture::new();
        fs::write(fixture.path("file"), b"fixture\n").unwrap();
        assert!(canonicalise(&fixture.path("file/"), true).is_err());
        assert!(canonicalise(&fixture.path("file/."), true).is_err());
        assert!(canonicalise(&fixture.path("missing/."), false).is_err());
        assert!(canonicalise(&fixture.path("missing/.//"), false).is_err());
        assert_eq!(
            canonicalise(&fixture.path("missing/"), false).unwrap(),
            fixture.path("missing")
        );
    }
}
