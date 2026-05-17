"""forkd — open-source fork-on-write microVM sandbox primitive.

This Python SDK provides two complementary surfaces:

- ``Sandbox`` (E2B-compatible) — connect to the in-guest agent of a
  running child VM to exec / eval code inside it.
- ``Controller`` — manage VM lifecycle (snapshots, sandboxes,
  branching) via the forkd-controller daemon's REST API.

Most agent runtimes use both: ``Controller`` to spawn / branch / kill,
``Sandbox`` to drive code execution inside one specific child.
"""

from .controller import Controller, ControllerError
from .sandbox import CommandResult, Sandbox

__version__ = "0.1.4"
__all__ = ["Sandbox", "CommandResult", "Controller", "ControllerError"]
