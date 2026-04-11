# remotefs

Mount remote filesystems locally using FUSE — with a local disk cache for fast repeated access.

| Binary         | Backend             |
| -------------- | ------------------- |
| `ssh_mount`    | SFTP over SSH       |
| `gdrive_mount` | Google Drive API v3 |

## Requirements

- **macOS**: [macFUSE](https://osxfuse.github.io/) installed at `/Library/Filesystems/macfuse.fs`
- **Linux**: `libfuse3-dev` and `libssh2-1-dev` packages
- Rust toolchain (edition 2024, MSRV 1.85)

## Build

```bash
  cargo build --release
```

## ssh_mount

Mount a remote directory over SFTP.

```bash
ssh_mount [OPTIONS] [USER@]HOST[:REMOTE_PATH] MOUNTPOINT
```

```bash
# Mount using an SSH config alias
ssh_mount myserver /mnt/remote

# Mount a specific path
ssh_mount root@192.168.1.1:/data /mnt/data

# Remote ~ is expanded via SFTP realpath
ssh_mount gce:~/projects /mnt/gce

# Explicit port and key
ssh_mount gce:/data /mnt/gce -p 2222 -i ~/.ssh/id_ed25519
```

**Options**

| Flag                       | Default                               | Description                     |
| -------------------------- | ------------------------------------- | ------------------------------- |
| `-p, --port PORT`          | `~/.ssh/config`, then 22              | SSH port                        |
| `-i, --identity FILE`      | —                                     | SSH identity file (private key) |
| `-c, --cache-path PATH`    | OS cache dir`/remotefs/<user>@<host>` | Local cache directory           |
| `-s, --cache-size MiB`     | 1024                                  | Max disk cache size             |
| `-t, --cache-timeout SECS` | 30                                    | Background refresh interval     |
| `-j, --parallel N`         | 4 × CPU cores                         | Worker thread count             |
| `--auto-cache`             | off                                   | Enable background cache refresh |
| `-v, --verbose`            | off                                   | Info-level logging              |

Host aliases, `HostName`, `Port`, `User`, and `IdentityFile` are resolved from `~/.ssh/config` via `ssh -G` before connecting. CLI flags take precedence.

**Default cache path**

| OS    | Path                                      |
| ----- | ----------------------------------------- |
| macOS | `~/Library/Caches/remotefs/<user>@<host>` |
| Linux | `$XDG_CACHE_HOME/remotefs/<user>@<host>`  |

## gdrive_mount

Mount Google Drive (or a sub-folder) as a local filesystem.

```bash
gdrive_mount [OPTIONS] ACCOUNT[:DRIVE_PATH] MOUNTPOINT
```

```bash
# Mount all of My Drive
gdrive_mount alice@gmail.com /mnt/gdrive

# Mount a sub-folder
gdrive_mount alice@gmail.com:/Projects /mnt/proj

# Two accounts simultaneously
gdrive_mount alice@gmail.com  /mnt/personal
gdrive_mount work@company.com /mnt/work

# Google Workspace Shared Drive
gdrive_mount alice@corp.com /mnt/shared --shared-drive 0ABCdef123XYZ

# Service account (no browser)
gdrive_mount sa@myproject.iam.gserviceaccount.com /mnt/drive -k ~/sa_key.json
```

**Options**

| Flag                       | Default                                      | Description                                               |
| -------------------------- | -------------------------------------------- | --------------------------------------------------------- |
| `-k, --credentials FILE`   | `~/.config/remotefs/gdrive_credentials.json` | OAuth2 credentials or service account key (auto-detected) |
| `--shared-drive ID`        | —                                            | Mount a Shared Drive instead of My Drive                  |
| `-c, --cache-path PATH`    | OS cache dir`/remotefs/gdrive_<email>`       | Local cache directory                                     |
| `-s, --cache-size MiB`     | 1024                                         | Max disk cache size                                       |
| `-t, --cache-timeout SECS` | 30                                           | Background refresh interval                               |
| `-j, --parallel N`         | 4 × CPU cores                                | Worker thread count                                       |
| `--auto-cache`             | off                                          | Enable background cache refresh                           |
| `-v, --verbose`            | off                                          | Info-level logging                                        |

### Setup: OAuth2 (desktop / personal use)

1. Create a Google Cloud project and enable the **Drive API**.
2. Go to **APIs & Services → Credentials**, create an OAuth2 client ID for a **Desktop app**, and download the JSON file.
3. Save it to `~/.config/remotefs/gdrive_credentials.json` (or pass `-k`).
4. On first run a browser window opens for authorization. The token is saved to `~/.config/remotefs/gdrive_token_<email>.json` and reused automatically.

### Setup: Service Account (headless / server use)

Service accounts authenticate without a browser — suitable for automated pipelines.

1. Enable the **Drive API** in your Google Cloud project.
2. Go to **IAM & Admin → Service Accounts**, create a service account, and download a JSON key.
3. Share the target Drive folder or Shared Drive with the service account's email.
4. Pass the key file with `-k`. The credential type is detected automatically.

### Shared Drives

Pass `--shared-drive <ID>` where `<ID>` is visible in the Drive URL (`/drive/folders/<ID>`).

### Trash

Deleting a file moves it to the Drive trash. The virtual `/.Trash` directory lists all trashed items:

```bash
ls  /mnt/gdrive/.Trash                          # view trashed files
mv  /mnt/gdrive/.Trash/report.docx /mnt/gdrive/ # restore
rm  /mnt/gdrive/.Trash/old_file                 # permanently delete
```

### Drive limitations

- **Google-native files** (Docs, Sheets, Slides) appear as 0-byte files; their content is not accessible.
- **Writes** upload the full cached file on each flush (Drive has no partial-update API).
- **Symlinks** are not supported (`ENOTSUP`).
- **POSIX attrs** (`chmod`, `chown`) are accepted but not persisted.

## How caching works

On first access a file is downloaded to a local disk cache. Subsequent reads are served from disk without any network round-trip. The cache is LRU-evicted once it reaches `--cache-size`.

Directory listings and file attributes are cached in memory (LRU). Both are persisted to disk on unmount and reloaded on the next mount, so a remount within 10 minutes requires no API calls for already-visited paths.

Cold directory listings return immediately (showing only `.` and `..`) and are populated in the background. The kernel is notified via FUSE inode invalidation once the listing arrives, triggering a transparent refresh.

## License

MIT
