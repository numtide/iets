# StructuredAttrs::parse then unparse: nlohmann writes the normal form
let d = j: (derivation { name = "jn"; system = "x"; builder = "/bin/sh"; __json = j; }).drvPath;
in [
  (d "{ \"b\" : 1, \"a\": [1, 2.50, null] }")
  (d "{\"a\":[1,2.5,null],\"b\":1}")
  (d "{\"a\":1,\"a\":2}")
  (d "{\"a\":2}")
  (d "{\"a\":1.0,\"b\":-0.0,\"c\":1e-7,\"d\":100000000000000000000,\"e\":1e15}")
  (d "{}")
  (d ''{"outPath":"x","keep":1}'')
  (d ''{"outPath":"x","keep":2}'')
  (d ''{"nested":{"__toString":0},"max":18446744073709551615,"\u0000":"\u0000"}'')
]
