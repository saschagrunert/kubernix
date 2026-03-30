# Run from repo root:
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

# Start the cluster
Type "sudo kubernix --nodes 10"
Enter
Sleep 10s

# Inside the kubernix shell, show cluster status
Type "kubectl get nodes"
Enter
Sleep 3s
