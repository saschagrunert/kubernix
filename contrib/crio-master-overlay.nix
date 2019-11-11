self: super: {
  cri-o = super.cri-o.overrideAttrs(old: {
    version = "master";
    src = super.fetchFromGitHub {
      owner = "cri-o";
      repo = "cri-o";
      rev = "9a322651bb25a5f15410d555a8d33bcb04d7cfcf"; # master: 11 Nov 2019 11:54:03 AM CET
      sha256 = "0q23l397fp0waj107jy8wf87fkq034lp8z736c5wmagnqlxca4iz";
    };
  });
}
