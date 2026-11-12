# Running Execute from cloud-init

This page shows how an Ubuntu server can install Execute and run one `.program`
file during unattended provisioning with cloud-init. It states only what the
repository provides today.

## What exists today

- The install path is the release tarball, checked against `SHA256SUMS` before
  anything is extracted. See [Releases and installation](releases.md). An apt
  repository for `packages:` is not available yet (issue #417), so the user-data
  below does not use `apt`.
- The release workflow does not build or publish a container image. There is no
  `ghcr.io` image to pull, and this page does not describe one.
- The example program is `examples/cloud-init/user-data.program`. It is a finite
  program: it runs `uname -a` and `df -h /` and nothing else.
- The run uses `--record`, so the run leaves a record directory. See
  [Record a run](guide.md#record-a-run-show-what-failed-and-replay-it). A run cannot
  combine `--record` with `--checkpoint` today: the interpreter refuses that
  combination for finite programs. This example therefore records the run and does
  not checkpoint it.

## User-data

The user-data below writes the program to `/srv/execute/user-data.program`, then
installs the release tarball, verifies its checksum, and runs the program once.
cloud-init runs `runcmd` once per instance. Replace `0.9.0-rc.4` with the release
you want. The `SHA256SUMS` file must come from the same release as the tarball.

```yaml
#cloud-config
write_files:
  - path: /srv/execute/user-data.program
    permissions: "0644"
    content: |
      # Purpose: Record a first-boot snapshot of an Ubuntu server during unattended provisioning.
      # Input: none; reads the kernel name and the root filesystem usage of this host.
      # Output: One kernel line and one disk usage table on standard output, kept in the run record.
      run "uname" with arguments ["-a"]
      then "df" with arguments ["-h", "/"]
runcmd:
  - [ sh, -c, "set -eu; VERSION=0.9.0-rc.4; BASE=https://github.com/monstercameron/NextGenScriptingLanguage/releases/download/v$VERSION; cd /tmp; curl -fsSLO $BASE/execute-$VERSION-linux-x64.tar.gz; curl -fsSLO $BASE/SHA256SUMS; sha256sum --check SHA256SUMS --ignore-missing" ]
  - [ sh, -c, "set -eu; tar -xzf /tmp/execute-0.9.0-rc.4-linux-x64.tar.gz -C /opt" ]
  - [ sh, -c, "set -eu; mkdir -p /var/lib/execute; /opt/execute-0.9.0-rc.4/bin/execute run /srv/execute/user-data.program --record /var/lib/execute/record" ]
```

The record directory must be new or empty. `execute run` refuses an existing
non-empty record directory, so a second cloud-init run on the same instance needs
a new directory name.

## Checks

- Checksum: `sha256sum --check SHA256SUMS --ignore-missing` must report `OK` for the
  tarball before extraction. A checksum shows that the file is intact and matches
  the published list. It does not show who produced the file. For a release that
  publishes `SHA256SUMS.sigstore.json`, verify the signature first, as described in
  [Releases and installation](releases.md).
- Record: the run writes `run.json`, `program.program`, `trace.jsonl` and `blobs/`
  under `/var/lib/execute/record`. Read it with `execute show-run` (see the guide
  link above).

## Not covered here

- Signed apt packages for `packages:` (issue #417).
- Container images, including an Ubuntu-based image with common tools.
- Checkpoint and resume for this program, which need the combination with
  `--record` that the interpreter does not yet accept.
- Booting a cloud image in continuous integration to run this user-data.
