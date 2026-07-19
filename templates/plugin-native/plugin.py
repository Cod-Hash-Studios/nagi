from __future__ import annotations

import json
import os


def main() -> None:
    context = json.loads(os.environ.get("NAGI_PLUGIN_CONTEXT_JSON", "{}"))
    workspace = context.get("workspace_label", "current workspace")
    print(f"{PLUGIN_NAME} is connected to {workspace}.")


if __name__ == "__main__":
    main()
