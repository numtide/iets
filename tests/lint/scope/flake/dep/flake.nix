{ outputs = { self }: { value = http://dep.example;
  mk = v: derivation { name = "wrapped"; system = "s"; builder = "b"; inherit v; };
  copy = p: "${p}";
}; }
