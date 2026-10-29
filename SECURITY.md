# Security

## Reporting a vulnerability

Report it privately through GitHub's private vulnerability reporting: open
the repository's **Security** tab and choose **Report a vulnerability**
([direct link](https://github.com/near/near-contract-sim/security/advisories/new)).
Please don't open a public issue or pull request for it. The maintainers
reply on the advisory, and credit you when it's published unless you ask
them not to.

## What to report here

- **Privately:** a simulator bug that can hide a contract bug, so that a
  test or audit passes when the same contract fails, loses funds or can be
  exploited on a real chain, or that can make the simulator run code or
  touch files outside what a test asked for.
- **As an ordinary issue:** any other simulator bug, including a
  difference from nearcore that can't mask a contract bug.
- **Not here:** a vulnerability you found *in a contract* with this tool.
  Report it to that contract's maintainers through their own disclosure
  process. A bug in nearcore itself goes to nearcore's security policy.

## Supported versions

Before 1.0 only the latest release, or `main` until the first one, gets
fixes.
