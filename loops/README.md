# Loops

The machinery that runs the building loops: the conductor service and the role prompts. The conductor receives board events, grants one lease per card or pull request, caps concurrent builders and daily cost, and starts one headless agent session per loop run with the prompt of its role.

`prompts/<role>.md` holds one prompt per role. This directory is a red zone: only a human changes it.

The conductor is not built yet.
