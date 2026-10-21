# Demo of the bonsai CLI. Record with demo/record.sh, which sets up the API and
# the cluster this tape talks to, and the helpers it calls off camera.

Output docs/assets/demo.gif

Set Shell "bash"
Set FontSize 22
Set Width 1600
Set Height 960
Set Padding 28
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms
Set CursorBlink false

# --- off camera: environment and a short prompt
Hide
Type "source $DEMO_ENV && PS1='$ ' && clear"
Enter
Sleep 500ms
Show

# --- 1. log in
Type "# Log in with an API token"
Enter
Sleep 400ms
Type "echo $TOKEN | bonsai login --with-token --store-file"
Enter
Sleep 2s
Type "bonsai whoami"
Enter
Sleep 2.5s

# --- 2. no card on file
Type "clear"
Enter
Type "# This account has no card on file yet"
Enter
Sleep 400ms
Type "bonsai cluster create shop --plan standard-fir --region aws-us-east-1"
Enter
Sleep 2s
# it offers to open the Checkout link; not now
Type "n"
Sleep 400ms
Enter
Sleep 3s

# --- off camera: a card is added through Checkout, and the bank declines it
Hide
Type "add_card_declined && clear"
Enter
Sleep 800ms
Show

# --- 3. the card is declined
Type "# A card was added, but the bank declines it"
Enter
Sleep 400ms
Type "bonsai cluster create shop --plan standard-fir --region aws-us-east-1"
Enter
Sleep 2s
Type "n"
Sleep 400ms
Enter
Sleep 3s

# --- 4. a good card: the create goes through and waits until ready
Type "clear"
Enter
Type "# With a working card, the cluster is created and the CLI waits until it's ready"
Enter
Sleep 400ms
Type "bonsai cluster create shop --plan standard-fir --region aws-us-east-1"
Enter
Sleep 4.5s

# --- off camera: point the cluster's host at the stand-in search server
Hide
Type "prepare_data shop && clear"
Enter
Sleep 2s
Show

# --- 5. load data and search it
Type "# Load a product catalogue into the new cluster"
Enter
Sleep 400ms
Type "export ES=$(bonsai url shop)"
Enter
Sleep 1s
Type `curl -s -XPOST "$ES/products/_bulk" -H 'Content-Type: application/x-ndjson' --data-binary @products.ndjson | jq '{took, errors, indexed: (.items | length)}'`
Enter
Sleep 3s
Type `curl -s "$ES/products/_search?q=boots" | jq '.hits.total.value, .hits.hits[0]._source'`
Enter
Sleep 3s

# --- off camera: the cluster's stats catch up with the load
Hide
Type "sync_stats shop && clear"
Enter
Sleep 1s
Show

# --- 6. watch it
Type "# See how the cluster is doing"
Enter
Sleep 400ms
Type "bonsai usage shop"
Enter
Sleep 3s
Type "bonsai metrics shop"
Enter
Sleep 4s
Type "bonsai clusters"
Enter
Sleep 3s
