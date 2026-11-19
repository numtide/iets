# qpaths.awk [-v fmt=job|query]: each job name (a.b."c.d".system) as the attribute path in the
# package set, without the system: a job `(at [ ... ]).drvPath`, a query `(q [ ... ])`
# fmt=fields reads fine-check's query id and tab-separated encoded names.
fmt == "fields" {
  out = ""
  for (i = 2; i <= NF; i++) {
    name = $i
    if (substr(name, 1, 1) == "~") {
      name = ""
      for (j = 2; j < length($i); j += 2) name = name sprintf("%c", strtonum("0x" substr($i, j, 2)))
    }
    out = out " \"" nix_chars(name) "\""
  }
  print "  (q [" out " ])"
  next
}
function nix_chars(s,    i,c,out) {
  out = ""
  for (i = 1; i <= length(s); i++) {
    c = substr(s, i, 1)
    if (c == "\\") out = out "\\\\"
    else if (c == "\"") out = out "\\\""
    else if (c == "\n") out = out "\\n"
    else if (c == "\r") out = out "\\r"
    else if (c == "\t") out = out "\\t"
    else if (substr(s, i, 2) == "${") out = out "\\$"
    else out = out c
  }
  return out
}
{
  n = 0; cur = ""; qd = 0
  for (i = 1; i <= length($0); i++) {
    c = substr($0, i, 1)
    if (c == "\"") { qd = !qd; continue }
    if (c == "." && !qd) { part[++n] = cur; cur = ""; continue }
    cur = cur c
  }
  part[++n] = cur
  if (fmt != "query") n--
  out = ""
  for (i = 1; i <= n; i++) out = out " \"" esc(part[i]) "\""
  if (fmt == "query") print "  (q [" out " ])"; else print "  (at [" out " ]).drvPath"
}
function esc(s) { gsub(/\\/, "\\\\", s); gsub(/"/, "\\\"", s); gsub(/\$/, "\\$", s); return s }
