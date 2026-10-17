## Change

Describe the problem and resulting behavior. Include a small before/after program when useful.

## Validation

List the checks actually run and their results. For runtime changes, cover relevant
byte, status, cancellation and state boundaries. For performance claims, link raw
paired release-build measurements and identify the environment.

## Compatibility

Describe platform differences, changed contracts or remaining gaps. Link the issue
and updated documentation when applicable.

## Security

- [ ] Touches a path, process, terminal, secret, or resource limit: I checked the
      [security checklist](../CONTRIBUTING.md#security-checklist) and the
      [threat model](../docs/threat-model.md) vector it affects.
- [ ] Any new `tests/security_*.rs` case passes, and any hole I found is listed in
      the threat model with its severity.
