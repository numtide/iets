# paths.awk: each job name (a.b."c.d".e) as a Nix list of its attribute names
{
  out = "  ["; cur = ""; q = 0
  for (i = 1; i <= length($0); i++) {
    c = substr($0, i, 1)
    if (c == "\"") { q = !q; continue }
    if (c == "." && !q) { out = out " \"" esc(cur) "\""; cur = ""; continue }
    cur = cur c
  }
  print out " \"" esc(cur) "\" ]"
}
function esc(s) { gsub(/\\/, "\\\\", s); gsub(/"/, "\\\"", s); gsub(/\$/, "\\$", s); return s }
