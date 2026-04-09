use crate::OutputFormat;

pub mod list {
    pub enum Kind {
        Local,
        All,
    }

    pub struct Options {
        pub kind: Kind,
    }
}

pub fn list(
    repo: gix::Repository,
    out: &mut dyn std::io::Write,
    format: OutputFormat,
    options: list::Options,
) -> anyhow::Result<()> {
    if format != OutputFormat::Human {
        anyhow::bail!("JSON output isn't supported");
    }

    let platform = repo.references()?;

    let (show_local, show_remotes) = match options.kind {
        list::Kind::Local => (true, false),
        list::Kind::All => (true, true),
    };

    if show_local {
        let mut branch_names: Vec<String> = platform
            .local_branches()?
            .flatten()
            .map(|branch| branch.name().shorten().to_string())
            .collect();

        branch_names.sort();

        for branch_name in branch_names {
            writeln!(out, "{branch_name}")?;
        }
    }

    if show_remotes {
        let mut branch_names: Vec<String> = platform
            .remote_branches()?
            .flatten()
            .map(|branch| branch.name().shorten().to_string())
            .collect();

        branch_names.sort();

        for branch_name in branch_names {
            writeln!(out, "{branch_name}")?;
        }
    }

    Ok(())
}
