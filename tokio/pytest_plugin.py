import asyncio
import contextlib
import re
import tempfile
import warnings

import pytest
from py import path

import tokio

from .test_utils import unused_port as _unused_port
from .test_utils import loop_context, setup_test_loop, teardown_test_loop

try:
    import uvloop
except:  # pragma: no cover
    uvloop = None


@contextlib.contextmanager
def _runtime_warning_context():
    """
    Context manager which checks for RuntimeWarnings, specifically to
    avoid "coroutine 'X' was never awaited" warnings being missed.

    If RuntimeWarnings occur in the context a RuntimeError is raised.
    """
    with warnings.catch_warnings(record=True) as _warnings:
        yield
        rw = ['{w.filename}:{w.lineno}:{w.message}'.format(w=w)
              for w in _warnings if w.category == RuntimeWarning]
        if rw:
            raise RuntimeError('{} Runtime Warning{},\n{}'.format(
                len(rw),
                '' if len(rw) == 1 else 's',
                '\n'.join(rw)
            ))


@contextlib.contextmanager
def _passthrough_loop_context(loop, fast=False):
    """
    setups and tears down a loop unless one is passed in via the loop
    argument when it's passed straight through.
    """
    if loop:
        # loop already exists, pass it straight through
        yield loop
    else:
        # this shadows loop_context's standard behavior
        loop = setup_test_loop()
        yield loop
        teardown_test_loop(loop, fast=fast)


def pytest_pycollect_makeitem(collector, name, obj):
    """
    Fix pytest collecting for coroutines.
    """
    if collector.funcnamefilter(name) and asyncio.iscoroutinefunction(obj):
        return list(collector._genfunctions(name, obj))


def pytest_pyfunc_call(pyfuncitem):
    """
    Run coroutines in an event loop instead of a normal function call.
    """
    if asyncio.iscoroutinefunction(pyfuncitem.function):
        existing_loop = pyfuncitem.funcargs.get('loop', None)
        with _runtime_warning_context():
            with _passthrough_loop_context(existing_loop, fast=False) as _loop:
                testargs = {arg: pyfuncitem.funcargs[arg]
                            for arg in pyfuncitem._fixtureinfo.argnames}

                task = _loop.create_task(pyfuncitem.obj(**testargs))
                _loop.run_until_complete(task)

        return True


def pytest_configure(config):
    LOOP_FACTORIES.clear()
    LOOP_FACTORY_IDS.clear()

    LOOP_FACTORIES.append(asyncio.new_event_loop)
    LOOP_FACTORY_IDS.append('pyloop')

    if uvloop is not None:  # pragma: no cover
        LOOP_FACTORIES.append(uvloop.new_event_loop)
        LOOP_FACTORY_IDS.append('uvloop')

    if tokio is not None:
        LOOP_FACTORIES.append(tokio.new_event_loop)
        LOOP_FACTORY_IDS.append('tokio')

    asyncio.set_event_loop(None)


LOOP_FACTORIES = []
LOOP_FACTORY_IDS = []


@pytest.yield_fixture(params=LOOP_FACTORIES, ids=LOOP_FACTORY_IDS)
def loop(request):
    """Return an instance of the event loop."""
    with loop_context(request.param, fast=False) as _loop:
        yield _loop


@pytest.yield_fixture(params=LOOP_FACTORIES, ids=LOOP_FACTORY_IDS)
def other_loop(request):
    """Return an instance of the event loop."""
    with loop_context(request.param, fast=False) as _loop:
        yield _loop


@pytest.fixture
def unused_port():
    """Return a port that is unused on the current host."""
    return _unused_port


@pytest.fixture
def shorttmpdir():
    """Provides a temporary directory with a shorter file system path than the
    tmpdir fixture.
    """
    tmpdir = path.local(tempfile.mkdtemp())
    yield tmpdir
    tmpdir.remove(rec=1)


class MockPattern(str):
    def __eq__(self, other):
        return bool(re.search(str(self), other, re.S))


@pytest.fixture
def mock_pattern():
    def pattern(s):
        return MockPattern(s)

    yield pattern


@pytest.fixture
def match():
    def regex_match(msg, pattern):
        return re.match(pattern, msg) is not None

    yield regex_match