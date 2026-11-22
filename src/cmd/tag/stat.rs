use std::{io::Write, path::Path};

use {anyhow::Context, lexopt::ValueExt};

use crate::{
    args::{self, Usage, flags, positional},
    datetime::DateTime,
    parallel::Parallel,
    tag::{Tag, Tagged},
};

const USAGE: &'static str = r#"
Tag file paths with datetime metadata. The datetimes come from a file's
last modified, last accessed or creation time.

File paths may be provided as positional arguments. Or, if there are no
positional arguments, then file paths are read from stdin as line delimited
data.

Note that it is not guaranteed that any particular metadata selection will
return correct or even "sensible" values. This largely depends on platform,
configuration and file system support. bttf just asks for the corresponding
metadata and uses it as given.

USAGE:
    bttf tag stat <kinds> <path>...
    bttf tag stat <kinds> < line delimited <path>

TIP:
    use -h for short docs and --help for long docs

EXAMPLES:
    Extract the datetime each file was created in a directory tree:

        find ./ | bttf tag stat created

REQUIRED ARGUMENTS:
%args%
OPTIONS:
%flags%
"#;

pub fn run(p: &mut lexopt::Parser) -> anyhow::Result<()> {
    let mut config = Config::default();
    let mut args = positional::Arguments::default();
    args::configure(p, USAGE, &mut [&mut config, &mut args])?;

    let kinds = config.metadata_kinds()?.to_vec();
    let mut wtr = std::io::stdout();
    // It's questionable whether parallelism is that useful
    // here. It does seem to help when multiple datetimes are
    // requested, but not so much when only one is.
    let mut parallel = Parallel::new(
        config.threads.get(),
        move |arg: positional::Argument<'static>| {
            let data = arg.original_with_line_terminator();
            let mut tagged = Tagged::new(data);
            let path = arg.into_path()?;
            for kind in kinds.iter() {
                let tag = Tag::new(kind.get(&path)?);
                tagged = tagged.tag(tag);
            }
            Ok(tagged.into_owned())
        },
        move |tagged: anyhow::Result<Tagged<DateTime>>| {
            tagged?.write(&mut wtr)?;
            writeln!(wtr)?;
            Ok(true)
        },
    );
    let result1 = args.try_map(|arg| parallel.send(arg.into_owned()));
    let result2 = parallel.wait();
    result1?;
    result2
}

#[derive(Clone, Copy, Debug)]
enum MetadataKind {
    Modified,
    Accessed,
    Created,
}

impl MetadataKind {
    fn get(&self, path: &Path) -> anyhow::Result<DateTime> {
        let md = std::fs::metadata(path)
            .with_context(|| path.display().to_string())?;
        let result = match *self {
            MetadataKind::Modified => {
                md.modified().context("failed to get last modified time")
            }
            MetadataKind::Accessed => {
                md.accessed().context("failed to get last accessed time")
            }
            MetadataKind::Created => {
                md.created().context("failed to get created time")
            }
        };
        let systime = result.with_context(|| path.display().to_string())?;
        let ts = jiff::Timestamp::try_from(systime)
            .with_context(|| path.display().to_string())?;
        let zdt = ts.to_zoned_datetime(jiff::tz::TimeZone::unknown());
        Ok(DateTime::from(zdt))
    }
}

impl std::str::FromStr for MetadataKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<MetadataKind> {
        Ok(match s {
            "modify" | "modified" => MetadataKind::Modified,
            "access" | "accessed" => MetadataKind::Accessed,
            "create" | "created" | "creation" | "birth" => {
                MetadataKind::Created
            }
            unk => anyhow::bail!("unknown file metadata kind: `{unk}`"),
        })
    }
}

#[derive(Debug, Default)]
struct Config {
    metadata_kinds: Vec<MetadataKind>,
    threads: flags::Threads,
}

impl Config {
    fn metadata_kinds(&self) -> anyhow::Result<&[MetadataKind]> {
        anyhow::ensure!(
            !self.metadata_kinds.is_empty(),
            "command requires at least one file metadata kind",
        );
        Ok(&self.metadata_kinds)
    }
}

impl args::Configurable for Config {
    fn configure(
        &mut self,
        p: &mut lexopt::Parser,
        arg: &mut lexopt::Arg,
    ) -> anyhow::Result<bool> {
        match *arg {
            lexopt::Arg::Short('j') | lexopt::Arg::Long("threads") => {
                self.threads = args::parse(p, "-j/--threads")?;
            }
            lexopt::Arg::Value(ref mut v) => {
                if !self.metadata_kinds.is_empty() {
                    return Ok(false);
                }
                let v = std::mem::take(v)
                    .string()
                    .context("metadata kind must be valid UTF-8")?;
                for kind in v.split(",") {
                    self.metadata_kinds.push(kind.parse()?);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn usage(&self) -> &[Usage] {
        const KINDS: Usage = Usage::arg(
            "<kinds>",
            "The kind of metadata to extract.",
            r#"
The kind of metadata to extract. This may be multiple kinds via comma separated
values of the following:

`modify` or `modified` extracts the last modified datetime of the file.

`access` or `accessed` extracts the last accessed datetime of the file.

`create`, `created`, `creation` or `birth` extracts the datetime that the file
was created.

When multiple kinds are requested, then they manifest as multiple tags for
each file path.
"#,
        );

        const PATH: Usage = Usage::arg(
            "<path>",
            "A file path to interpolate into <command>.",
            r#"
A file path to interpolate into <command>.

File paths must be passed on stdin in a line delimited format.

If an <arg> contains `{}`, then it is replaced with the file path. Otherwise,
the file path is added to the end of the command invocation.
"#,
        );

        &[KINDS, PATH, flags::Threads::USAGE]
    }
}
