def pytest_addoption(parser):
    parser.addoption("--run-t3", action="store_true", default=False, help="run T3 mtu1500 counterexample (default skip)")
