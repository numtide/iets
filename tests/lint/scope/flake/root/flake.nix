{
  inputs.dep.url = "path:@DEP@";
  outputs = { self, dep }: {
    value = dep.value + http://root.example;
    file = "${./flake.nix}";
    dir = "${./sub}";
    inputFile = import (dep + "/file.nix");
    store = "${/. + "@STORE@"}";
    meta = derivation { name = "meta"; system = "s"; builder = "b"; v = "1.${builtins.substring 0 8 self.lastModifiedDate}"; args = [ self.narHash ]; };
    wrapped = dep.mk self.narHash;
    depMeta = derivation { name = "dep-meta"; system = "s"; builder = "b"; v = dep.narHash; };
    # iets: allow tree-meta-in-drv
    allowed = derivation { name = "allowed"; system = "s"; builder = "b"; v = self.narHash; };
    inputCopy = dep.copy (/. + "@DEP@/../plain");
  };
}
