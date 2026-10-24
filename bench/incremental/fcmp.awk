# fcmp.awk -v f=FILE A.tab B.tab: the regions and literal names of FILE that differ between two revisions
BEGIN { FS = OFS = "\t" }
FNR == 1 { side++ }
$1 == "E" { err[side] = 1; next }
$1 == "R" { h[side, $3] = $4; if (side == 1) regs[$3] = 1; next }
$1 == "L" { k[side, $3 "\t" $4] = $5; if (side == 1) lits[$3 "\t" $4] = 1; next }
END {
  if (err[1]) { print "CF", f; exit }
  for (r in regs) if (err[2] || !((2, r) in h) || h[2, r] != h[1, r]) print "CR", f, r
  for (l in lits) {
    if (err[2] || !((2, l) in k)) { print "CL", f, l, "*"; continue }
    if (k[1, l] == k[2, l]) continue
    delete a; delete b; d = ""
    n = split(k[1, l], x, " "); for (i = 1; i <= n; i++) a[x[i]] = 1
    n = split(k[2, l], x, " "); for (i = 1; i <= n; i++) { b[x[i]] = 1; if (!(x[i] in a)) d = d " " x[i] }
    for (s in a) if (!(s in b)) d = d " " s
    print "CL", f, l, substr(d, 2)
  }
}
