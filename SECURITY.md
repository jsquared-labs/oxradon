# Security

## Reporting a vulnerability

Please report it privately, through GitHub's
[vulnerability reporting form](https://github.com/jsquared-labs/oxradon/security/advisories/new),
not in a public issue. Include the oxradon version, the command, and a file or steps that show
the problem.

Reports are answered on a best-effort basis. A confirmed problem is fixed in a new release and
then disclosed in a GitHub security advisory, with credit to you if you want it.

## What counts

oxradon reads Python source and configuration files and never runs them. Please report
privately anything that breaks that, for example:

- code in an analyzed file, or a setting in a configuration file, that oxradon executes;
- oxradon reading, writing or deleting files other than the paths it was given, the
  configuration files it reads, and the output file named by `-O` or by a configuration file's
  `output_file`;
- a published package (crates.io or PyPI) that doesn't match what the release workflow built
  from the tagged source.

A crash, a hang, heavy memory use or wrong output on some input is a normal bug. Please open a
[public issue](https://github.com/jsquared-labs/oxradon/issues/new/choose) for it. Two limits
are already documented in COMPATIBILITY.md: code nested beyond about 100,000 levels exhausts the
stack, and `--exclude` and `--ignore` patterns are capped at 1 GiB once compiled.

## Supported versions

Only the latest release gets fixes.
