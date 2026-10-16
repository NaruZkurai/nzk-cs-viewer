# `core/roslyn/` — the offline validity drivers

`ParseCheck.cs` is a tiny Roslyn driver with no NuGet dependencies. The Rust
engine embeds it with `include_str!` (`compile_check::ROSLYN_DRIVER_SRC`) and
can build it on demand with the .NET SDK's own Roslyn, so a dev checkout needs
nothing shipped beside the binary.

## Two ways the driver runs

1. **Built on demand (dev / CLI).** `compile_check::ensure_roslyn_driver`
   compiles this source with
   `<dotnet-root>/sdk/*/Roslyn/bincore/csc.dll` and the
   `Microsoft.NETCore.App.Ref` pack into a content-addressed cache under
   `$CARGO_TARGET_DIR/csgraph-roslyn` (or `$CSGRAPH_ROSLYN_BUILD`), then runs
   it with `dotnet`.
2. **Prebuilt bundle (shipped AppImage).** `build-appimage.sh` compiles the same
   source once and stages the result into
   `usr/share/csviewer/roslyn/`. `compile_check::parse_check_with` prefers that
   bundle when it is complete, and `build-appimage.sh` also bundles the .NET
   host runtime under `usr/lib/csviewer-dotnet/` (unless `--no-bundle-dotnet`).

## Bundle contract

A driver directory is usable only when it contains **all** of
`DRIVER_BUNDLE_FILES` (`compile_check::DRIVER_BUNDLE_FILES`):

```
ParseCheck.dll                     # the compiled driver
ParseCheck.runtimeconfig.json      # copied from csc.runtimeconfig.json
Microsoft.CodeAnalysis.dll         # Roslyn, from .../Roslyn/bincore/
Microsoft.CodeAnalysis.CSharp.dll
ParseCheck.cs                      # source, for transparency/reproducibility
```

Search order (`compile_check::driver_bundle_dirs`), most explicit first:

1. `$CSGRAPH_ROSLYN_DRIVER` — a directory; if set it must be complete, else the
   gate reports `skipped` naming the missing files (it never silently falls
   through).
2. `$CSVIEWER_HOME/usr/share/csviewer/roslyn` (AppRun sets `CSVIEWER_HOME`).
3. `$APPDIR/usr/share/csviewer/roslyn`.
4. `<exe-dir>/../share/csviewer/roslyn` for a `usr/bin/<app>` layout.
5. `<exe-dir>/roslyn`.

The dotnet host is `$CSGRAPH_DOTNET`, then a host shipped beside the bundle
(`usr/lib/csviewer-dotnet/dotnet`, or `dotnet` inside the bundle dir), then
`dotnet` from `PATH`/well-known locations. When no host or no complete bundle
exists, the gate returns `ok:false` + a concrete `skipped:<reason>` — never a
false pass.

## Environment knobs

| knob | meaning |
|---|---|
| `CSGRAPH_DOTNET` | `dotnet` executable to use |
| `CSGRAPH_ROSLYN_DRIVER` | explicit prebuilt driver directory |
| `CSGRAPH_ROSLYN_BUILD` | where the on-demand driver is cached |
| `CSVIEWER_HOME` / `APPDIR` | app root used to find the bundle |

`build-appimage.sh` also honours `CSVIEWER_BUNDLE_DOTNET` (`auto`/`0`/`1`) and
the `--bundle-dotnet` / `--no-bundle-dotnet` flags.
