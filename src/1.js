
(async () => {
            return await new Promise((resolve) => {
                // --- 配置区 (可根据网速调整) ---
                const scrollStep = 500;   // 每次下移 80px (越大越快，但太大会漏图，建议 50-100)
                const frequency = 16;    // 每 16ms 动一次 (模拟 60fps 顺滑动画)
                const waitTime = 1000;   // 触底后等待多久(ms)确认真的没图了
                // ---------------------------

                let totalHeight = 0;
                let noChangeTicks = 0;
                // 计算需要等待多少个 tick (频率是 16ms，所以 2000ms / 16ms ≈ 125 次)
                const maxTicks = waitTime / frequency; 

                const timer = setInterval(() => {
                    const scrollHeight = document.body.scrollHeight;
                    const currentPos = window.scrollY + window.innerHeight;

                    // 1. 执行滚动
                    // 这里不加 behavior: smooth，因为我们通过 setInterval 自己实现了物理上的 smooth
                    window.scrollBy(0, scrollStep);

                    // 2. 检测是否触底 (留 50px 容差)
                    if (currentPos >= scrollHeight - 50) {
                        noChangeTicks++;

                        // 如果高度变了（加载出新图了），重置计数器
                        if (scrollHeight > totalHeight) {
                            totalHeight = scrollHeight;
                            noChangeTicks = 0;
                        }

                        // 如果连续 N 次循环高度都没变，说明真的到底了
                        if (noChangeTicks >= maxTicks) {
                            clearInterval(timer);
                            
                            // 3. 抓取结果
                            let images = document.querySelectorAll('img');
                            let titles = document.querySelectorAll('');
                            let urls = [];
                            let len = 0;
                            let names = []
                            images.forEach((img) => {
                                // 优先 data-src，其次 src
                                let url = img.getAttribute('data-src') || img.getAttribute('src');
                                let name = 
                                if (url) urls.push(url);
                            });
                            
                            const data ={

                            }


                            resolve(JSON.stringify(urls));
                        }
                    } else {
                        // 还没到底，重置计数器
                        if (scrollHeight > totalHeight) {
                            totalHeight = scrollHeight;
                        }
                        noChangeTicks = 0;
                    }
                }, frequency);
            });
        })();