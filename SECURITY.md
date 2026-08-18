# Security Policy

## Supported versions

Security fixes are provided for the latest released minor version of Flint.
Before the first release, fixes are applied to the default branch.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability.

Email [security@ameba.ai](mailto:security@ameba.ai) with:

- A description of the issue and its potential impact.
- The affected Flint version or commit.
- Reproduction steps or a proof of concept.
- Any suggested mitigation, if available.

We will acknowledge reports within three business days and coordinate disclosure
with the reporter. Please allow time for investigation and a release before
publishing details.

## Security model

Flint is a local development utility, not a production isolation boundary.
Access to the Docker socket grants effectively host-level Docker control. Run
Flint only with trusted runtime images and on trusted development systems.
