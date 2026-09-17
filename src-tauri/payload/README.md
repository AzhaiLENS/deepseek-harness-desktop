# Payload

`scripts/vendor-runtime.sh` writes the self-contained runtime archive here:

```
dsh-payload-<platform>-<arch>.tar.zst   # node + the full @deepseek-ai/dsh closure
dsh-payload-<platform>-<arch>.json      # sha256 + version stamp
```

The files are ~100 MB each and fully reproducible, so they are **not** committed
(see `.gitignore`). Build one for the current machine with:

```bash
node scripts/make-icons.mjs                 # once, generates src-tauri/icons
bash scripts/vendor-runtime.sh              # writes ../dist/dsh-payload-<host>.tar.zst
cp ../dist/dsh-payload-* .                  # stage it for the Tauri bundler
```

`scripts/dev.sh` does all of that in one step.
