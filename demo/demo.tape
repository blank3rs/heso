# VHS tape — renders demo.gif headlessly.
# Run from repo root:
#   vhs demo/demo.tape

Output demo/demo.gif

Set FontSize 14
Set Width 1100
Set Height 720
Set Padding 16
Set Theme "Dracula"
Set Shell "powershell"
Set TypingSpeed 35ms

# Banner pause so viewers see the prompt before typing starts.
Sleep 800ms

Type "python demo/agent.py"
Enter
Sleep 600ms

# Type the query into the agent's prompt.
Type "find three rust web scraping libraries and summarize"
Sleep 400ms
Enter

# Hold while the agent runs search -> batch read -> answer.
Sleep 16s
