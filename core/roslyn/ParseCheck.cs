// ParseCheck — a tiny offline Roslyn parse-diagnostics driver.
//
// Built on demand by `csgraph_core::compile_check` using the .NET SDK's own
// `Roslyn/bincore/csc.dll` (see `build_roslyn_driver`). It is embedded into the
// Rust binary with `include_str!`, written into the (gitignored) cargo target
// tree, compiled there, and run with `dotnet`.
//
// It uses only Microsoft.CodeAnalysis.CSharp's `CSharpSyntaxTree.ParseText`, so
// what it reports is a real Roslyn syntax diagnostic (never a hand-rolled
// guess). Exit code is 0 whenever the driver ran at all; the caller decides
// validity from the emitted `diagnostics` array.

using System;
using System.IO;
using System.Text;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;

internal static class ParseCheck
{
    private static string Escape(string s)
    {
        var b = new StringBuilder(s.Length + 8);
        foreach (char c in s)
        {
            switch (c)
            {
                case '"': b.Append("\\\""); break;
                case '\\': b.Append("\\\\"); break;
                case '\n': b.Append("\\n"); break;
                case '\r': b.Append("\\r"); break;
                case '\t': b.Append("\\t"); break;
                default:
                    if (c < 0x20 || c > 0x7e) b.Append("\\u").Append(((int)c).ToString("x4"));
                    else b.Append(c);
                    break;
            }
        }
        return b.ToString();
    }

    private static int Main(string[] args)
    {
        string version = typeof(CSharpSyntaxTree).Assembly.GetName().Version?.ToString() ?? "unknown";
        var sb = new StringBuilder();
        int fileCount = 0, diagCount = 0;
        sb.Append("{\"version\":\"").Append(Escape(version))
          .Append("\",\"diagnostics\":[");
        foreach (string path in args)
        {
            string text;
            try
            {
                if (!File.Exists(path)) continue;
                text = File.ReadAllText(path);
            }
            catch (Exception)
            {
                // an unreadable file is not a syntax diagnostic; the caller can
                // tell from the file count that it was not parsed
                continue;
            }
            fileCount++;
            var tree = CSharpSyntaxTree.ParseText(text, path: path);
            foreach (var d in tree.GetDiagnostics())
            {
                var pos = d.Location.GetLineSpan();
                if (diagCount > 0) sb.Append(',');
                diagCount++;
                sb.Append("{\"severity\":\"").Append(d.Severity.ToString().ToLowerInvariant())
                  .Append("\",\"id\":\"").Append(Escape(d.Id))
                  .Append("\",\"file\":\"").Append(Escape(path))
                  .Append("\",\"line\":").Append(pos.StartLinePosition.Line + 1)
                  .Append(",\"col\":").Append(pos.StartLinePosition.Character + 1)
                  .Append(",\"message\":\"").Append(Escape(d.GetMessage())).Append("\"}");
            }
        }
        sb.Append("],\"files\":").Append(fileCount)
          .Append(",\"count\":").Append(diagCount).Append('}');
        Console.WriteLine(sb.ToString());
        return 0;
    }
}
