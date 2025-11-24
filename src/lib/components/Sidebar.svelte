<script>
    import { Button } from "svelte-foundry";

	// Add any necessary imports or logic here
</script>

<aside class="sidebar p-8 min-w-52">
	<Button class="blue">asdasd</Button>
</aside>

<style lang="scss">
	@use 'svelte-foundry/colors' as colors;

	.sidebar {
		background-color: colors.$background-mid;
		color: colors.$text-mute;
		border-right: 2px solid colors.$background-light;
	}
</style>
