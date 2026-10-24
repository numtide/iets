# fcheck.awk [-v skip=REGEX] fchg.all TRACES: for each job, each step that changed something its fine trace observed.
# fchg.all holds "step\tchange" lines; TRACES holds "J\tjob" and then the job's trace.
# skip drops the whole reads (A) whose Nix call or stack matches it: what a fix at those sites would give.
BEGIN { FS = OFS = "\t" }
NR == FNR {
  s = $1
  if ($2 == "CR") cr[$3 "\t" $4] = cr[$3 "\t" $4] " " s
  else if ($2 == "CF") cf[$3] = cf[$3] " " s
  else if ($2 == "CL") { cl[$3 "\t" $4 "\t" $5] = cl[$3 "\t" $4 "\t" $5] ";" s ":" $6; n = split($6, x, " "); for (i = 1; i <= n; i++) cn[x[i]] = 1 }
  else if ($2 == "CD") { cd[$3] = cd[$3] ";" s ":" $4; cn[$4] = 1 }
  else if ($2 == "CM") cm[$3] = cm[$3] " " s
  else if ($2 == "CT") ct[$3] = ct[$3] " " s
  else if ($2 == "CS") cs[$3] = cs[$3] " " s
  else if ($2 == "CX") cx[$3] = cx[$3] ";" s ":" $4
  next
}
function mark(ss, why,    n, x, i) { n = split(ss, x, " "); for (i = 1; i <= n; i++) if (!(x[i] in inv)) inv[x[i]] = why }
function entries(e, name, why,    n, x, i, y) {
  n = split(e, x, ";")
  for (i = 2; i <= n; i++) {
    split(x[i], y, ":")
    if (name == "" || y[2] == "*" || index(" " y[2] " ", " " name " ")) mark(y[1], why)
  }
}
function flush(   s) { if (job != "") for (s in inv) print job, s, inv[s]; delete inv; delete pc }
$1 == "J" { flush(); job = $2; next }
$1 == "R" { k = $2 "\t" $3; if (k in cr) mark(cr[k], "R " $2 " " $3); if ($2 in cf) mark(cf[$2], "R " $2); next }
$1 == "P" {
  if ($3 == "lit") pc[$2] = cl[$4 "\t" $5 "\t" $6]
  else if ($3 == "dir") pc[$2] = cd[$4]
  else if ($3 == "u") pc[$2] = pc[$4] pc[$5]
  else if ($3 == "x") pc[$2] = pc[$4]
  else print job, "?", "unknown provenance " $0
  next
}
$1 == "K" { if (pc[$2] != "" && ($3 in cn || index(pc[$2], ":*"))) entries(pc[$2], $3, "K " $3); next }
$1 == "A" { if (pc[$2] != "" && !(skip != "" && ($4 " " $5) ~ skip)) entries(pc[$2], "", "A " $3 " " $4); next }
$1 == "X" { if ($2 in cx) { n = split(cx[$2], x, ";"); for (i = 2; i <= n; i++) { split(x[i], y, ":"); if (y[2] + 0 <= $3 + 0) mark(y[1], "X " $2) } } next }
$1 == "F" || $1 == "C" { if ($2 in cm) mark(cm[$2], $1 " " $2); next }
$1 == "T" { if ($2 in ct) mark(ct[$2], "T " $2); next }
$1 == "S" { if ($2 in cs) mark(cs[$2], "S " $2); next }
$1 == "D" { if ($2 in cd) entries(cd[$2], "", "D " $2); next }
END { flush() }
