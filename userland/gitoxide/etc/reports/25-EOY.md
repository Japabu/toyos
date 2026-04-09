##### The year in numbers

And 365 days later as of 2025-12-31, we are counting **211,983 SLOC, up by 33,827**, which is 91% *of the year before* (➡*OTYB*) in **14845 commits up by 1,302** and 57%OTYB. There are **65 crates (clearly I miscounted last year)** and 2 binaries, with `ein` and `gix` as part of `gitoxide`. There are **186 unique authors (up by 35 and 76%OTYB)**. This means ~93 lines per day in ~3.5 commits each day. On GitHub there are **10695 stars (up by 1,416 which is 70%OTYB)** for ~3.9 stars per day.

The tool invocation `ein tool estimate-hours` now rates the project cost at **12284 hours (up by 1655 which is 87%OTYB) or ~1535 x 8-hour working days**, for an average working time of **4.20 hours in the past 365 days**.

My time-tracker reveals that I **spent 461h on open source work** (which is 52%OTYB) which is (still) dominated by `gitoxide` and which is supported [_via GitHub sponsors_](https://github.com/sponsors/Byron) at 287h (which is 46%OTYB). **2177h were for commercial work and consulting** (which is 213%OTYB). The **total of 2015 hours worked** boils down to **5.5 hours of work per day (38.75h per week)**, which is 105%OTYB.

My open-source work is still financially sustainable even without income through commercial work, which is something I am most grateful for.

Thus far, **I have spent the last 2084 days to getting `gitoxide` off the ground** (5.7y!), and it's still quite far from even reaching parity with `git2` despite making great strides. Even though feature-wise it's getting closer, it's still unclear how to get everything to stability while also making the API easy-to-use, yet powerful enough to squeeze out every last bit of performance.

### Plans and reality

For this year, there were no plans which definitely helps with not feeling unaccomplished. Looking back, I don't think I worked on any bigger feature, and it feels all like maintenance, improvements and of course, reviewing the various contributions which at least *felt* more numerous this year.
They must be the reason that the line count increase is *only* 91% of what it was last year - would it just have been me, I think it would have been 10%.

When looking back at the accomplishments of last year I am absolutely shocked that these are already one year old, many feel much more recent. Time flies.

Also, out of the back of my head, I don't recall any major contribution of mine. What's going on?

### Words of Gratitude

By now I am able to sustain myself and my family while following my calling, and for that I am incredibly grateful - I simply couldn't imagine a better use of my (life)time. Doing so would not be possible without the generosity of my sponsors and the trust of my clients: thank you, thank you *very much*!

Another big thanks goes to all the contributors which by now do most of the work, with special shout-outs to [Christoph Rüßler](https://github.com/cruessler) and [Eliah Kagan](https://github.com/EliahKagan).

#### Thanks, GitHub!

GitHub provides useful resources and advice, and we've had the opportunity to learn from their experts, which [Eliah Kagan](https://github.com/EliahKagan) has found to be especially helpful. This will continue to lead to further improvements in the coming months.

GitHub also provides Copilot for free, which is incredibly useful to me, so special-thanks for that!

#### Thanks, Meta!

Meta sponsored `gitoxide` with 20.000USD in OpenCollective, and I am using it for paid maintenance now. This helps tremendously in putting more hours in maintenance, given that these can now compete with paid work.

Thank you 🙏!

#### Thank you, Josh!

[Josh Triplett](https://joshtriplett.org/), back in May 2021 became my first sponsor and *patron*, which did no less than change my life to be able to follow my calling. `gitoxide`, me and my family wouldn't be what they are today without him, and I am deeply grateful for that. Nothing ever has to change about this sentence.

As in 2025 I didn't manage to contribute all that much to [`buildit`](http://buildit.dev), and I don't know to what extent this can change next year.

Thanks for bearing with me!

#### Thank you, Cargo team, for bearing with me!

It's taking me years to finish the integration work and implement all features needed to fully replace `git2` in `cargo`, and yet the `cargo` team stays onboard with this work!

A bare-bones `reset` is definitely planned for 2026, and I will do my best to integrate it once it becomes available.

#### Thanks, GitButler!

Last but not least, let me thank GitButler and the wonderful people involved with it for bearing with me, particularly when I seem to be able to deliver any new feature in weeks 😅. But it's getting there, and early 2026 will be the year when the platform will stabilize, to be able to carry the future.

#### Thanks Everyone

It’s very likely that I failed to call *you* out for no other reason than swiss-cheese like memory, so let me thank you for the net-positive interactions we undoubtedly had.

Let’s do that again in 2026 :)!

----

🎉🎉🎉 Thanks for reading, let's make 2026 a great year for everyone :)! 🎉🎉🎉

----

<details><summary>Data</summary>

##### State

```shell
❯ git rev-parse main
30d8d5c3992e70a0361301e659d6e25de1fbd4b4
```

##### Commits

```shell
❯ git rev-list --count  main
14848
```

##### Linecount

```
❯ tokei
===============================================================================
 Language            Files        Lines         Code     Comments       Blanks
===============================================================================
 JSON                    1            7            7            0            0
 Makefile                1          156          110           10           36
 Shell                 179        14556        11977          691         1888
 SVG                     3           43           43            0            0
 Plain Text             38          729            0          585          144
 TOML                   95         4341         3203          424          714
-------------------------------------------------------------------------------
 HTML                    1          327          324            0            3
 |- CSS                  1           12            2           10            0
 |- JavaScript           1            1            1            0            0
 (Total)                            340          327           10            3
-------------------------------------------------------------------------------
 Markdown              154       102600            0        75827        26773
 |- Dockerfile           1            4            3            0            1
 |- Python               1           10            6            2            2
 |- Rust                 2           64           61            0            3
 |- Shell                5           39           37            2            0
 (Total)                         102717          107        75831        26779
-------------------------------------------------------------------------------
 Rust                 1364       211990       190072         2294        19624
 |- Markdown           846        20339            7        17260         3072
 (Total)                         232329       190079        19554        22696
===============================================================================
 Total                1836       334749       205736        79831        49182
===============================================================================
```

##### Authors

```shell
❯ ein t hours
 17:27:41 traverse commit graph done 14.8K commits in 0.10s (149.8K commits/s)
 17:27:41        estimate-hours Extracted and organized data from 14849 commits in 59.834µs (248169936 commits/s)
total hours: 12286.61
total 8h days: 1535.83
total commits = 14849
total authors: 186
total unique authors: 179 (3.76% duplication)
```

</details>
