import json
import shlex
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateDefaultsTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    fake_codex = fixtures.DelegateTests.fake_codex
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def user_config(self, value):
        directory = self.work / 'config'
        self.env['XDG_CONFIG_HOME'] = str(directory)
        path = directory / 'delegate/config.json'
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value))

    def meta(self, state):
        return json.loads((Path(state['dir']) / 'meta.json').read_text())

    def test_defaults_merge_cli_override_and_in_place(self):
        repo = self.repo({'keep.txt': 'safe'})
        self.user_config({'cleanupKeepExecutables': ['user-cache'], 'defaults': {'worktree': True, 'protect': ['keep.txt'],
                                      'acceptAlso': ['echo user'], 'timeout': '2m', 'evidence': 'echo proof'}})
        (repo / '.delegate.json').write_text(json.dumps({'accept': 'true', 'defaults': {'timeout': '3m'}, 'cleanupKeepExecutables': ['repo-cache', 'user-cache']}))
        self.fake_pi([fixtures.answer('ok'), fixtures.SETTLED])
        result = self.cli('run', '--workdir', repo, '检查默认配置')
        state = self.outcome(result)
        meta = self.meta(state)
        self.assertTrue(meta['worktree'])
        self.assertEqual(meta['timeoutSeconds'], 180)
        self.assertEqual(meta['protect'], ['keep.txt'])
        self.assertEqual(meta['cleanupKeepExecutables'], ['user-cache', 'repo-cache'])
        self.assertEqual(meta['accept'], 'true && echo user')
        self.assertEqual(meta['evidenceCommand'], 'echo proof')
        self.assertEqual(meta['defaults']['worktree']['source'], 'user')
        self.assertEqual(meta['defaults']['timeout']['source'], 'repo')
        self.assertIn('生效默认值及来源', result.stderr)
        reply = self.outcome(self.cli('reply', '--wait', state['run'], '--timeout', '4m',
                                      '--accept', 'true', '--no-evidence', 'follow up'))
        self.assertEqual(set(reply['defaults']), {'worktree', 'protect'})
        second = self.outcome(self.cli('run', '--workdir', repo, '--in-place', '--timeout', '4m',
                                      '--protect', '.delegate.json', '--accept-also', 'echo cli',
                                      '--no-evidence', 'override'))
        meta = self.meta(second)
        self.assertIsNone(meta['worktree'])
        self.assertEqual(meta['protect'], ['.delegate.json'])
        self.assertEqual(meta['accept'], 'true && echo cli')
        self.assertEqual(meta['timeoutSeconds'], 240)
        self.assertNotIn('evidenceCommand', meta)
        self.assertEqual(meta['defaults'], {})

    def test_read_only_defaults_do_not_add_write_contracts(self):
        self.user_config({'defaults': {'worktree': True, 'protect': ['safe'], 'acceptAlso': ['false'],
                                      'evidence': 'false', 'timeout': '2m'}})
        self.fake_pi([fixtures.answer('ok'), fixtures.SETTLED])
        state = self.outcome(self.cli('run', '--read-only', 'read'))
        meta = self.meta(state)
        self.assertEqual(meta['timeoutSeconds'], 120)
        self.assertIsNone(meta['accept'])
        self.assertEqual(meta['protect'], [])
        self.assertNotIn('evidenceCommand', meta)

    def test_standing_layers_modes_deny_and_reply(self):
        repo = self.repo({'a.txt': 'a'})
        self.user_config({'standing': {'all': ['follow AGENTS.md', 'common'], 'write': 'write rule', 'readOnly': 'read rule'},
                          'agentDeny': [{'argv': ['cargo', 'check'], 'hint': 'use focused tests'}]})
        (repo / '.delegate.json').write_text(json.dumps({'standing': {'all': 'repo common'}}))
        self.fake_pi([fixtures.answer('ok'), fixtures.SETTLED])
        result = self.cli('run', '--workdir', repo, 'first')
        state = self.outcome(result)
        self.assertEqual(state['standingSources']['all'], 'repo')
        self.assertEqual(state['standingSources']['write'], 'user')
        self.assertEqual(state['standingSources']['agentDeny'], ['user'])
        self.assertIn('write rule', state['standing'])
        self.assertNotIn('read rule', state['standing'])
        self.assertIn('这些命令被拦截：cargo check [prefix]（use focused tests）', state['standing'])
        self.assertIn('附加 3 行', result.stderr)
        prompt = (Path(state['dir']) / 'prompt.md').read_text()
        self.assertTrue(prompt.rstrip().endswith(state['standing']))
        reply = self.cli('reply', '--wait', state['run'], 'next')
        second = self.outcome(reply)
        self.assertEqual(second['standing'], state['standing'])
        self.assertNotIn('repo common', (Path(second['dir']) / 'prompt.md').read_text())
        self.assertIn('附加 0 行', reply.stderr)
        fresh = self.outcome(self.cli('reply', '--wait', '--fresh', second['run'], 'fresh'))
        self.assertIn('repo common', (Path(fresh['dir']) / 'prompt.md').read_text())
        read = self.outcome(self.cli('run', '--read-only', '--workdir', repo, 'inspect'))
        self.assertIn('read rule', read['standing'])
        self.assertNotIn('write rule', read['standing'])

    def test_standing_string_and_array(self):
        self.fake_pi([fixtures.answer('ok'), fixtures.SETTLED])
        for standing in ['line one\nline two', ['line one', 'line two']]:
            with self.subTest(standing=standing):
                self.user_config({'standing': standing})
                state = self.outcome(self.cli('run', '--read-only', 'task'))
                self.assertEqual(state['standing'], 'line one\nline two')

    def test_generated_name_first_line_and_uniqueness(self):
        self.fake_pi([fixtures.answer('ok'), fixtures.SETTLED])
        first = self.outcome(self.cli('run', '--read-only', '--prompt', '# 审查接口\nmore detail'))
        self.assertEqual(first['name'], '审查接口')
        self.assertTrue(first['run'].endswith('-审查接口'))
        second = self.outcome(self.cli('run', '--read-only', '--prompt', '# 审查接口\nmore detail'))
        self.assertNotEqual(first['run'], second['run'])
        reply = self.outcome(self.cli('reply', '--wait', first['run'], 'next'))
        self.assertEqual(reply['name'], 'reply to 审查接口')

    def test_generated_name_stops_at_the_first_clause(self):
        for prompt, name in (
            ('对话文件夹补充（ADR 0085 第 3、5 节）：前两段已在主干', '对话文件夹补充'),
            ('目标：实现上传，只做后端', '实现上传'),
            ('Goal: fix the flaky wait test', 'fix the flaky wait test'),
            ('Review the error handling in apps/api, list real defects only', 'Review the error'),
            ('一二三四五六七八九十一二三四五六七八九十一二三四五六七八九十', '一二三四五六七八九十一二三四五六七八九十一二三四'),
        ):
            with self.subTest(prompt=prompt):
                self.fake_pi([fixtures.answer('ok'), fixtures.SETTLED])
                self.assertEqual(self.outcome(self.cli('run', '--read-only', '--prompt', prompt))['name'], name)

    def test_display_limit_tolerance_and_environment_priority(self):
        self.user_config({'resultChars': 100})
        # result.md adds one newline, so use 124 answer characters for the exact boundary.
        self.fake_pi([fixtures.answer('字' * 124), fixtures.SETTLED])
        full = self.cli('run', '--read-only', 'boundary-full')
        self.assertNotIn('省略', full.stdout)
        self.fake_pi([fixtures.answer('字' * 130), fixtures.SETTLED])
        truncated = self.cli('run', '--read-only', 'over')
        self.assertIn('省略', truncated.stdout)
        self.env['DELEGATE_RESULT_CHARS'] = '200'
        self.assertNotIn('省略', self.cli('wait', self.outcome(truncated)['run']).stdout)
        repo = self.repo({'a.txt': 'a'})
        (repo / '.delegate.json').write_text(json.dumps({'resultChars': 200}))
        del self.env['DELEGATE_RESULT_CHARS']
        state = self.outcome(self.cli('run', '--read-only', '--workdir', repo, 'repo limit'))
        self.assertEqual(state['resultDisplayChars'], 200)

    def test_new_builtin_display_limit(self):
        self.fake_pi([fixtures.answer('字' * 6817), fixtures.SETTLED])
        result = self.cli('run', '--read-only', 'long')
        self.assertNotIn('省略', result.stdout)
        self.assertEqual(self.outcome(result)['resultDisplayChars'], 20000)

    def test_auto_compression_once_preserves_original_and_session(self):
        original = '结论证据' * 10
        self.fake_pi([fixtures.answer(original), fixtures.SETTLED], [fixtures.answer('结论：证据'), fixtures.SETTLED])
        result = self.cli('run', '--read-only', '--max-answer', '10', 'compress')
        state = self.outcome(result)
        run = Path(state['dir'])
        self.assertEqual((run / 'result.md').read_text().strip(), '结论：证据')
        self.assertEqual((run / 'result-original.md').read_text(), original)
        self.assertTrue(state['compression']['ok'])
        self.assertIn('答复不超过 10 字', (run / 'prompt.md').read_text())
        self.assertIn('压缩到 10 字以内，保留结论与证据', (run / 'compression-prompt.md').read_text())
        self.assertIn('--fork', (self.work / 'pi.log').read_text().splitlines()[1])
        self.assertIn(str(run / 'result-original.md'), result.stdout)
        self.assertEqual(state['attempts'], 2)
        reply = self.outcome(self.cli('reply', '--wait', state['run'], 'next'))
        self.assertEqual(reply['maxAnswer'], 10)

    def test_compression_failure_keeps_original_and_budget(self):
        repo = self.repo({'a.txt': 'a'})
        self.user_config({'maxRework': 0})
        self.fake_pi([fixtures.answer('x' * 40), fixtures.SETTLED], [fixtures.answer('y' * 40), fixtures.SETTLED])
        result = self.cli('run', '--workdir', repo, '--max-answer', '10', 'compress')
        state = self.outcome(result)
        self.assertEqual(state['state'], 'answered')
        self.assertFalse(state['compression']['ok'])
        self.assertEqual((Path(state['dir']) / 'result.md').read_text().strip(), 'x' * 40)
        self.assertEqual(len((self.work / 'pi.log').read_text().splitlines()), 2)
        self.assertIn('失败，保留原答复', result.stdout)

    def test_compression_failure_to_execute_keeps_original(self):
        # No thread.started event: there is no session to continue.
        events = fixtures.codex_events('x' * 40)[1:]
        self.fake_codex(events)
        state = self.outcome(self.cli('run', '--agent', 'codex', '--read-only', '--max-answer', '10', 'task'))
        self.assertFalse(state['compression']['ok'])
        self.assertIn('no saved session', state['compression']['error'])
        self.assertEqual((Path(state['dir']) / 'result.md').read_text().strip(), 'x' * 40)
        self.assertEqual(len((self.work / 'pi.log.codex').read_text().splitlines()), 1)

    def test_compression_codex_continues_latest_session(self):
        compact = ' '.join(shlex.quote(json.dumps(e)) for e in fixtures.codex_events('short'))
        self.fake_codex(fixtures.codex_events('x' * 40),
                        pre=f'if [ "$(wc -l < "$PI_LOG.codex")" -gt 1 ]; then printf "%s\\n" {compact}; exit 0; fi')
        state = self.outcome(self.cli('run', '--agent', 'codex', '--read-only', '--max-answer', '10', 'task'))
        self.assertTrue(state['compression']['ok'])
        self.assertIn('exec fork t1', (self.work / 'pi.log.codex').read_text())

    def test_compression_only_above_one_and_half(self):
        self.fake_pi([fixtures.answer('x' * 15), fixtures.SETTLED])
        state = self.outcome(self.cli('run', '--read-only', '--max-answer', '10', 'task'))
        self.assertNotIn('compression', state)
        self.assertEqual(len((self.work / 'pi.log').read_text().splitlines()), 1)

    def test_invalid_config_and_options_report_field(self):
        for config, field in [({'standing': {'other': 'x'}}, 'standing'),
                              ({'standing': ['x', 1]}, 'standing'), ({'resultChars': 0}, 'resultChars'),
                              ({'defaults': {'timeout': 'bad'}}, 'defaults.timeout'),
                              ({'defaults': {'protect': 'path'}}, 'defaults.protect'),
                              ({'cleanupKeepExecutables': ['bad/name']}, 'cleanupKeepExecutables')]:
            with self.subTest(config=config):
                self.user_config(config)
                self.fake_pi([fixtures.answer('ok'), fixtures.SETTLED])
                result = self.cli('run', '--read-only', 'task')
                self.assertEqual(result.returncode, 2)
                self.assertIn(field, result.stderr)
        self.assertEqual(self.cli('start', '--max-answer', '0', 'task').returncode, 2)
        self.assertEqual(self.cli('start', '--in-place', '--worktree', 'task').returncode, 2)

    def test_invalid_user_protect_is_not_hidden_by_repo_or_cli(self):
        repo = self.repo({'a.txt': 'a'})
        self.user_config({'defaults': {'protect': ['../outside']}})
        (repo / '.delegate.json').write_text(json.dumps({'defaults': {'protect': ['a.txt']}}))
        self.fake_pi([fixtures.answer('ok'), fixtures.SETTLED])
        result = self.cli('run', '--workdir', repo, '--protect', 'a.txt', 'task')
        self.assertEqual(result.returncode, 2)
        self.assertIn('config/delegate/config.json: invalid defaults.protect', result.stderr)
