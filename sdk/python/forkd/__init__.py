"""forkd — open-source fork-on-write microVM sandbox primitive.

This Python SDK provides an E2B-compatible Sandbox API. Under the hood,
each Sandbox is a forked microVM child managed by the `forkd` Rust CLI.
"""

from .sandbox import CommandResult, Sandbox

__version__ = "0.1.3"
__all__ = ["Sandbox", "CommandResult"]
