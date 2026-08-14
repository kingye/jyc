import os
p = 'crates/jyc-channels/src/github/inbound/tests.rs'
lines = open(p).read().split('\n')
assert lines[0].strip() == '#[cfg(test)]'
assert lines[1].strip() == 'mod inbound_tests {'
body = lines[2:]
while body and body[-1].strip() == '':
    body.pop()
assert body[-1].strip() == '}'
body = body[:-1]
# de-indent (the body was nested inside the mod wrapper)
body = [l[4:] if l.startswith('    ') else l for l in body]
# drop leading blanks
while body and body[0].strip() == '':
    body.pop(0)
# the body's import block is its first lines until the first non-use line
import_end = 0
while import_end < len(body) and (body[import_end].startswith('use ') or body[import_end].strip() == ''):
    import_end += 1
import_block = body[:import_end]
rest = body[import_end:]
# find the shared helpers
helper_names = ['make_message', 'make_patterns', 'make_message_with_rules', 'make_ci_test_config']
helpers = []
tests = []
i = 0
while i < len(rest):
    l = rest[i]
    if any(l.strip().startswith('fn ' + n + '(') for n in helper_names):
        # capture the fn until its closing brace at 0-indent
        depth = 0
        fn_lines = []
        while i < len(rest):
            fn_lines.append(rest[i])
            depth += rest[i].count('{') - rest[i].count('}')
            i += 1
            if depth == 0:
                break
        helpers.extend(fn_lines)
        helpers.append('')
        continue
    tests.append(l)
    i += 1
# split tests into two halves at a #[test] boundary
mid = len(tests) // 2
best = None
for k in range(mid, min(mid + 200, len(tests))):
    if tests[k].strip() == '#[test]':
        best = k
        break
if best is None:
    for k in range(mid, max(0, mid - 200), -1):
        if tests[k].strip() == '#[test]':
            best = k
            break
assert best is not None
part1 = tests[:best]
part2 = tests[best:]
os.makedirs('crates/jyc-channels/src/github/inbound/tests', exist_ok=True)
open('crates/jyc-channels/src/github/inbound/tests/mod.rs', 'w').write(
    '#[cfg(test)]\nmod inbound_tests;\n#[cfg(test)]\nmod inbound_tests_part2;\n')
open('crates/jyc-channels/src/github/inbound/tests/inbound_tests.rs', 'w').write(
    '\n'.join(import_block + [''] + helpers + [''] + part1) + '\n')
helper_import = 'use super::inbound_tests::{' + ', '.join(helper_names) + '};'
open('crates/jyc-channels/src/github/inbound/tests/inbound_tests_part2.rs', 'w').write(
    '\n'.join(import_block + [helper_import, ''] + part2) + '\n')
os.remove(p)
print('split done: helpers', len(helpers), 'part1', len(part1), 'part2', len(part2))
