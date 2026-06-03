# fuz

Fast fuzzy text-file searching.

> [!WARNING]
> This is competely vibe-coded. I did not read the sourcecode. Use at your own risk!

Like ripgrep, but with fuzzy matching instead of regular expression matching.

## Installing

`cargo install --git https://github.com/acidtv/fuz fuz`

## Usage

```
fuz [-n N] [--no-file-limit] [--no-line-limit] PATTERN
```

- `-n N` — return at most N results (default 20).
- `--no-file-limit` — search files larger than 10 MiB (skipped by default).
- `--no-line-limit` — search lines longer than 64 KiB (skipped by default).

## Ranking

Matches are ranked in three tiers, best first:

1. **Literal** — the needle appears verbatim in the line.
2. **Within-word subsequence** — needle chars appear in order inside a single word (e.g. `rqrs` in `requires`).
3. **Cross-word subsequence** — needle chars span multiple words (e.g. `clscntrdiv` matching `class CountryDivision`).

Within a tier, alignments that hit more **word boundaries** rank higher. A boundary is the start of the line, a position right after a separator (`_`, `-`, `.`, space, …), or a CamelCase transition (lowercase followed by uppercase, like `r`→`D` in `CountryDivision`). The bonus extends forward through any contiguous run of matched chars that follows — so `div` matching `D-i-v` together inside `Division` is rewarded across all three chars, while `div` scattered across `D`etail+`V`iew is rewarded only at the isolated boundaries. Tighter alignments — fewer wasted bytes between matched needle chars — also rank higher.

Case-sensitivity is *smart*: an uppercase char anywhere in the needle makes the search case-sensitive; otherwise it's case-insensitive.

## Example

Example searching the Linux kernel source:
```bash
linux-7.1-rc6$ time fuz bpflstdta
kernel/bpf/bpf_cgrp_storage.c:35:15:static struct bpf_local_storage_data *
kernel/bpf/bpf_cgrp_storage.c:52:9:     struct bpf_local_storage_data *sdata;
kernel/bpf/bpf_cgrp_storage.c:69:9:     struct bpf_local_storage_data *sdata;
kernel/bpf/bpf_cgrp_storage.c:86:9:     struct bpf_local_storage_data *sdata;
kernel/bpf/bpf_cgrp_storage.c:128:9:    struct bpf_local_storage_data *sdata;
kernel/bpf/bpf_inode_storage.c:35:15:static struct bpf_local_storage_data *inode_storage_lookup(struct inode *inode,
kernel/bpf/bpf_inode_storage.c:78:9:    struct bpf_local_storage_data *sdata;
kernel/bpf/bpf_inode_storage.c:91:9:    struct bpf_local_storage_data *sdata;
kernel/bpf/bpf_inode_storage.c:107:9:   struct bpf_local_storage_data *sdata;
kernel/bpf/bpf_inode_storage.c:128:9:   struct bpf_local_storage_data *sdata;
kernel/bpf/bpf_local_storage.c:462:37:static int check_flags(const struct bpf_local_storage_data *old_sdata,
kernel/bpf/bpf_local_storage.c:546:8:struct bpf_local_storage_data *
kernel/bpf/bpf_local_storage.c:550:9:   struct bpf_local_storage_data *old_sdata = NULL;
kernel/bpf/bpf_task_storage.c:30:15:static struct bpf_local_storage_data *
kernel/bpf/bpf_task_storage.c:63:9:     struct bpf_local_storage_data *sdata;
kernel/bpf/bpf_task_storage.c:95:9:     struct bpf_local_storage_data *sdata;
kernel/bpf/bpf_task_storage.c:131:9:    struct bpf_local_storage_data *sdata;
kernel/bpf/bpf_task_storage.c:171:9:    struct bpf_local_storage_data *sdata;
net/core/bpf_sk_storage.c:20:15:static struct bpf_local_storage_data *
net/core/bpf_sk_storage.c:37:9: struct bpf_local_storage_data *sdata;
fuz bpflstdta  1.73s user 0.94s system 1131% cpu 0.236 total
```
