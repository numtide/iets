let xy = 42; in
[
  (rec { ${''x''\y''} = 1; b = xy; }).b
  (rec { ${''x''\y''}.v = 1; b = xy; }).b
  (rec { ${''${"xy"}''} = 1; b = xy; }).b
  (if true then 3 else { "''t" = 2; ${'''''t''} = 1; })
]
