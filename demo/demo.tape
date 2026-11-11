# Demo of the bonsai CLI, for the README. Record with demo/record.sh, which sets
# up the API and the cluster this tape talks to, and the helpers it calls off camera.

Output docs/assets/demo.gif

Set Shell "bash"
Set FontSize 20
Set Width 1200
Set Height 720
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms
Set CursorBlink false

# --- off camera: environment, a short prompt, and a login
Hide
Type "source $DEMO_ENV && PS1='$ ' && demo_login && clear"
Enter
Sleep 1.5s
Show
Sleep 500ms

# --- 1. create a cluster and wait until it's ready
Type "bonsai cluster create shop --plan standard-fir --region aws-us-east-1"
Enter
Sleep 4s

# --- 2. put its connection URL where the app reads it
Type "bonsai cluster url shop --write .env"
Enter
Sleep 2s

# --- off camera: a week of history and some data, then erase this line
Hide
Type "add_history shop; printf '\033[1A\033[2K'"
Enter
Sleep 3s
Show

# --- 3. watch it
Type "bonsai cluster usage shop"
Enter
Sleep 3s
Type "bonsai cluster list"
Enter
Sleep 4s
