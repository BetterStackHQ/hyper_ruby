# HyperRuby

A very niche, fast HTTP server for Ruby, but not intended for general-purpose use.

It's not compatible with rack or rails. You probably don't want to use it if you're after a general-purpose web server for your app.

## Development

After checking out the repo, run `bin/setup` to install dependencies. Then, run `rake test` to run the tests. You can also run `bin/console` for an interactive prompt that will allow you to experiment.

To install this gem onto your local machine, run `bundle exec rake install`. To release a new version, update the version number in `version.rb`, and then run `bundle exec rake release`, which will create a git tag for the version, push git commits and the created tag, and push the `.gem` file to [rubygems.org](https://rubygems.org).

## Syslog listeners

Alongside HTTP, the server can accept syslog over a stream socket (Unix socket
or TCP, optionally behind a PROXY v2 header) and over UDP. Each complete message
is yielded to the same `Server#run_worker` block as requests, as a
`HyperRuby::SyslogMessage` the block answers with an admission verdict. See the
configuration keys documented at the top of `lib/hyper_ruby.rb`.

## License

The gem is available as open source under the terms of the [MIT License](https://opensource.org/licenses/MIT).

`ext/hyper_ruby/src/syslog/framing.rs` is an exception: it is a port of
Vector 0.48.0's `lib/codecs/src/decoding/framing/octet_counting.rs` and is
licensed under the [Mozilla Public License 2.0](https://www.mozilla.org/en-US/MPL/2.0/),
which is why the gem's metadata lists both licences.

## Code of Conduct

Everyone interacting in the HyperRuby project's codebases, issue trackers, chat rooms and mailing lists is expected to follow the [code of conduct](https://github.com/[USERNAME]/hyper_ruby/blob/master/CODE_OF_CONDUCT.md).
