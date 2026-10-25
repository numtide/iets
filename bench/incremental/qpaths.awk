# qpaths.awk [-v fmt=job|query]: each job name (a.b."c.d".system) as the attribute path in the
# package set, without the system: a job `(at [ ... ]).drvPath`, a query `(q [ ... ])`
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
