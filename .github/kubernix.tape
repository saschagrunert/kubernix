# Run from repo root after a first `kubernix --nodes 10` bootstrap, so that
# the recording shows a warm start:
#   vhs .github/kubernix.tape
#   nix run nixpkgs#vhs -- .github/kubernix.tape
Output .github/kubernix.gif
Output .github/kubernix.webm

Set Shell "bash"
Set FontSize 14
Set Width 660
Set Height 310
Set Padding 10
Set Theme "Dracula"
Set BorderRadius 10
Set TypingSpeed 70ms

# Use the local build and a plain sh prompt inside the kubernix shell
Hide
Type "export PATH=$PWD/target/release:$PATH KUBERNIX_SHELL=sh && clear"
Enter
Show

# Start the cluster
Type "kubernix --nodes 10"
Enter
Wait+Screen@3m /Spawning interactive shell/
Sleep 1s

# Inside the kubernix shell, show cluster status
Type "kubectl get nodes"
Enter
Sleep 3s

# Stop the cluster
Hide
Type "exit"
Enter
Wait@2m
