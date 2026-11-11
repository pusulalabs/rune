- **A closing shortcut host leaves a later host's handlers alone.** Closing used to clear every
  handler in the App, so a host that attached while the previous one was closing could
  register a chord successfully and then have it do nothing when pressed. Each registration now
  records the host that took it, and a close removes the handlers of that host and those it
  replaced, plus those of whatever registrations the engine it let go of held. A later host's
  other handlers stay, as do those of registrations no host has taken yet. A removed handler is
  now dropped after the locks around it are released, so a handler whose captures call back
  into the App can't deadlock a registration.
