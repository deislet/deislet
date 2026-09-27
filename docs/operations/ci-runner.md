# The CI runner on s7

`.github/workflows/checks.yml` runs every job on a self-hosted runner:

```yaml
runs-on: [self-hosted, linux, x64, deislet]
```

GitHub-hosted runners stopped starting for this organisation on 2026-09-04
(「recent account payments have failed or your spending limit needs to be
increased」), so the jobs now run on the build host s7, which already has
what the workspace needs: rustup, sccache with its distributed workers
(vm2064/vm2065), mold, and the canonical target directories under
`/srv/targets`. Until the runner described here is registered, every run of
the workflow waits in the queue for a runner that does not exist.

This page is the whole procedure. Everything runs on s7; the steps marked
**admin** need an owner of the `deislet` GitHub organisation.

## What the workflow expects of the host

| Needed by | What |
|---|---|
| every job | `git`, `bash`, `curl`, the runner service itself |
| governance | `python3` with the `venv` module (`python3-venv` on Ubuntu) |
| unit files | `systemd-analyze` |
| cargo jobs | `/usr/local/bin/rust-target-env`, rustup for the runner user, `sccache` on `PATH`, `clang` and `mold` (the linker shim `/usr/local/bin/rust-link-mold`) |
| demo | `jq`, `openssl`, `iproute2`, `python3` |

Node 22 comes from `actions/setup-node` and grpcurl from its release page;
both land in the runner's tool cache on the first run and are reused after.
`cargo-audit` and `cargo-nextest` are installed by the workflow at pinned
versions into the runner user's `~/.cargo/bin` the first time, and found there
afterwards. The rust toolchain is whatever `rust-toolchain.toml` pins; the
`build-host` action (`.github/actions/build-host/action.yml`) installs it if
it is missing.

## 1. A dedicated user

The runner executes whatever a branch's workflow and build scripts say, so it
runs as its own user: no sudo, no docker group, no access to anybody's home.

```bash
sudo useradd --create-home --shell /bin/bash gh-runner
```

## 2. The user's Rust environment

rustup, with no toolchain yet (the first job installs the pinned one):

```bash
sudo -u gh-runner -H sh -c \
  'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none'
```

The same cargo configuration developers on s7 use — the mold linker shim,
`incremental = false`, `debug = "line-tables-only"`, the job count. The
workflow deliberately sets none of these itself: they are part of sccache's
cache key, and a CI that set them differently would never share a cached
compilation with anything.

```bash
sudo install -o gh-runner -g gh-runner -m 0644 /home/l/.cargo/config.toml /home/gh-runner/.cargo/config.toml
```

The sccache client configuration, which names the distributed scheduler and
carries its token. It is a credential for the build cluster, so it is
readable by the runner user alone:

```bash
sudo install -d -o gh-runner -g gh-runner -m 0700 /home/gh-runner/.config/sccache
sudo install -o gh-runner -g gh-runner -m 0600 /home/l/.config/sccache/config /home/gh-runner/.config/sccache/config
```

The runner user gets its own sccache server and local cache
(`~gh-runner/.cache/sccache`); it does not share the local cache with user
`l`. Compilations are still distributed to the same workers.

## 3. Target directories

Every cargo step runs after the `build-host` action, which evaluates
`/usr/local/bin/rust-target-env` in the checkout and exports the resulting
`CARGO_TARGET_DIR` through `GITHUB_ENV` for the rest of the job. The script
names the directory after the repository directory. `/srv/targets/.owners/deislet`
already belongs to the developer checkout (`/home/l/D/deislet/deislet`), so
the runner's checkout (`/home/gh-runner/actions-runner/_work/deislet/deislet`)
gets its own path-qualified name, `deislet-<hash>`:

- real storage: `/srv/targets/deislet-<hash>`
- visible path: `/build/deislet-<hash>/target`

That separation is intended. Two Unix users writing one target directory
fight over file ownership, and CI would queue behind every developer's and
agent's cargo lock. What CI needs from the canonical directory is that it is
the *same* directory on every run, because sccache's keys include the target
path: a fresh directory per run is a cold build every run.

The script has to be able to create those entries, and today `/srv/targets`
and `/build` are writable by `l` alone. Give both users a common group:

```bash
sudo groupadd --force rustbuild
sudo usermod -aG rustbuild l
sudo usermod -aG rustbuild gh-runner
sudo chgrp rustbuild /srv/targets /srv/targets/.owners /build
sudo chmod 2775 /srv/targets /srv/targets/.owners /build
```

(`l` has to log in again for the new group to apply to its own sessions; the
runner picks it up when its service starts in step 5.)

If the script cannot create the directory it prints nothing, and the
`build-host` action fails the job with a pointer to this section rather than
letting cargo fall back to a `target/` inside the checkout.

## 4. Register the runner (admin token)

1. **admin** Open
   <https://github.com/organizations/deislet/settings/actions/runners/new>,
   choose Linux / x64, and copy the registration token from the
   `./config.sh` line. The token is valid for one hour.
2. On the same page, copy the download and checksum commands for the current
   runner release. Run them as the runner user:

   ```bash
   sudo -iu gh-runner
   mkdir actions-runner && cd actions-runner
   # From the page, verbatim:
   curl -o actions-runner-linux-x64-<VERSION>.tar.gz -L \
     https://github.com/actions/runner/releases/download/v<VERSION>/actions-runner-linux-x64-<VERSION>.tar.gz
   echo "<SHA256>  actions-runner-linux-x64-<VERSION>.tar.gz" | shasum -a 256 -c
   tar xzf ./actions-runner-linux-x64-<VERSION>.tar.gz
   ```

3. Configure it for the organisation, with the custom label. `self-hosted`,
   `Linux` and `X64` are added automatically, and labels match without regard
   to case, so the runner carries exactly the four labels the workflow asks
   for:

   ```bash
   ./config.sh --unattended \
     --url https://github.com/deislet \
     --token <REGISTRATION_TOKEN> \
     --name s7 \
     --labels deislet \
     --work _work
   ```

4. The environment the jobs see. The service does not read a login shell, so
   `PATH` and the sccache wrapper are written into the runner's own files:

   ```bash
   echo "/home/gh-runner/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" > .path
   cat >> .env <<'EOF'
   RUSTC_WRAPPER=sccache
   SCCACHE_CACHE_SIZE=64G
   EOF
   exit
   ```

Register **one** runner with the `deislet` label. A runner takes one job at a
time, and that is what keeps two cargo builds of this workspace from running
on s7 at once (the workflow's own concurrency group only cancels superseded
runs of the same ref).

## 5. Run it as a systemd service

```bash
cd /home/gh-runner/actions-runner
sudo ./svc.sh install gh-runner
sudo ./svc.sh start
sudo ./svc.sh status
```

The unit is `actions.runner.deislet.s7.service`; its log is
`journalctl -u actions.runner.deislet.s7.service`. The runner updates itself
when GitHub publishes a new release.

## 6. First run

Trigger the workflow by hand (Actions → checks → Run workflow, or
`gh workflow run checks.yml --ref main`). In the `workspace` and `demo` jobs,
the `build-host` step prints the `CARGO_TARGET_DIR` it chose; it should be
`/build/deislet-<hash>/target`. The first run compiles the workspace cold
and installs the pinned cargo tools; later runs reuse both.

## Security

- **Private repository only.** A self-hosted runner runs the code of every
  workflow run it accepts, with the runner user's files — here including a
  token for the build cluster. Never make this repository public while the
  runner is attached, and do not attach the runner to public repositories.
  If the organisation's plan allows restricting a runner group to selected
  repositories, restrict the group this runner is in (Settings → Actions →
  Runner groups) to `deislet/deislet`; if it does not, and more repositories
  are added to the organisation, register the runner at repository level
  instead (same steps, with
  <https://github.com/deislet/deislet/settings/actions/runners/new> and
  `--url https://github.com/deislet/deislet`).
- **No workflows from forks.** Under Settings → Actions → General, leave
  「Run workflows from fork pull requests」 off for private repositories, and
  consider turning off 「Allow forking of private repositories」 altogether.
  The workflow enforces the same thing on its own: every job has
  `if: github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository`,
  and `scripts/check-product-governance.py` refuses a job without it.
- **Least privilege on the host.** The runner user has no sudo and is not in
  the `docker` group; `svc.sh install` needs root once, the service then runs
  as `gh-runner`. The workflow's `GITHUB_TOKEN` is read-only
  (`permissions: contents: read`).
- **Nothing lives in /tmp.** Jobs write scratch files under `RUNNER_TEMP`
  and tools under `RUNNER_TOOL_CACHE`, both inside the runner's directory.

## Removing the runner

```bash
cd /home/gh-runner/actions-runner
sudo ./svc.sh stop
sudo ./svc.sh uninstall
# admin: a removal token from the runner's page in the organisation settings
sudo -u gh-runner ./config.sh remove --token <REMOVAL_TOKEN>
```

Its target directory is `/srv/targets/deislet-<hash>`; remove it together
with `/build/deislet-<hash>` and the claim file of the same name in
`/srv/targets/.owners/`.
