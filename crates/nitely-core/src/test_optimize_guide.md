# Classify the flagged tests

This is the analysis step of /nitely-test-optimize. It reads code; it changes nothing. Time is an input here: a test costs what it takes, and it is worth what it would catch.

1. `nitely test-sweep map report.json` lists the tests the evidence flags, as a JSON template to fill: each slow (by one measuring method), flaky or failing test, each test whose body waits on time or asserts UI text or presence, and each lead the collector found (a body the same as another's, heavy mocking). Every entry has its path and line, its median time when timed, why it is flagged, and mechanical hints: counts of waits, UI assertions, and process, git, network and database calls in its body. A hint is a lead, never a verdict.
2. Read each flagged test and the code it calls, in whatever language the repo uses, and give it one class:
   - `decision`: it checks one decision. Name the decision in a few words (`decision`) and the module that makes it (`module`). These become refactor cards, one per module: lift the decision into a plain function over plain data, test that without I/O, and keep one or two scenarios for the module.
   - `scenario`: it proves real I/O end to end (a process, git, the network, a database) and nothing cheaper can. Keep it, named in the test budget's scenarios.
   - `presence`: it asserts that UI text or an element exists. No test should only check that a button or a label is there: delete it, or check the behaviour through state.
   - `wall-clock`: it waits on the clock or on machine load. That is a defect: wait on the event instead, or inject the clock.
   - `duplicate`: another test checks the same thing. Name it in `of`.
   - `restates`: it restates the implementation (mirrors its steps, pins private details or mock calls) instead of checking a behaviour.
   - `keep`: it is worth what it costs as it is, and the hint that flagged it was a false lead (a variable named `html`, a timeout that is an argument, not a wait). Its `note` must say why.
   Add a `note` when the class needs a reason the next person would ask for.
3. `nitely test-sweep map report.json --classes classes.json --output <scratch>/test-map.md` checks that every flagged test has exactly one class and nothing else is classified, and writes the map: decisions grouped by module with their measured time, then the scenarios kept and the tests to delete or rewrite. Keep it in scratch. It goes into the repo's docs only once the person approves the goal built from it, as one change with the first refactor.
4. Nothing is deleted or refactored here. The map feeds the goal proposal, where the person approves refactors module by module and deletions once, each deletion naming what covers it.
