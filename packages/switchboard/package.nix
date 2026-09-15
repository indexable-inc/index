{
  id = "switchboard";
  packageSet = true;
  flake = true;
  overlay = false;
  passthruTests = {
    prefix = "switchboard";
  };
}
